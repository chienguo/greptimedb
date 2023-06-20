// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod heartbeat;
mod load_balance;
mod lock;
mod router;
mod store;

use api::v1::meta::Role;
use common_grpc::channel_manager::{ChannelConfig, ChannelManager};
use common_meta::rpc::lock::{LockRequest, LockResponse, UnlockRequest};
use common_meta::rpc::router::{CreateRequest, DeleteRequest, RouteRequest, RouteResponse};
use common_meta::rpc::store::{
    BatchDeleteRequest, BatchDeleteResponse, BatchGetRequest, BatchGetResponse, BatchPutRequest,
    BatchPutResponse, CompareAndPutRequest, CompareAndPutResponse, DeleteRangeRequest,
    DeleteRangeResponse, MoveValueRequest, MoveValueResponse, PutRequest, PutResponse,
    RangeRequest, RangeResponse,
};
use common_telemetry::info;
use heartbeat::Client as HeartbeatClient;
use lock::Client as LockClient;
use router::Client as RouterClient;
use snafu::{OptionExt, ResultExt};
use store::Client as StoreClient;

pub use self::heartbeat::{HeartbeatSender, HeartbeatStream};
use crate::error;
use crate::error::{ConvertMetaRequestSnafu, ConvertMetaResponseSnafu, Result};

pub type Id = (u64, u64);

#[derive(Clone, Debug, Default)]
pub struct MetaClientBuilder {
    id: Id,
    role: Role,
    enable_heartbeat: bool,
    enable_router: bool,
    enable_store: bool,
    enable_lock: bool,
    channel_manager: Option<ChannelManager>,
}

impl MetaClientBuilder {
    pub fn new(cluster_id: u64, member_id: u64, role: Role) -> Self {
        Self {
            id: (cluster_id, member_id),
            role,
            ..Default::default()
        }
    }

    pub fn enable_heartbeat(self) -> Self {
        Self {
            enable_heartbeat: true,
            ..self
        }
    }

    pub fn enable_router(self) -> Self {
        Self {
            enable_router: true,
            ..self
        }
    }

    pub fn enable_store(self) -> Self {
        Self {
            enable_store: true,
            ..self
        }
    }

    pub fn enable_lock(self) -> Self {
        Self {
            enable_lock: true,
            ..self
        }
    }

    pub fn channel_manager(self, channel_manager: ChannelManager) -> Self {
        Self {
            channel_manager: Some(channel_manager),
            ..self
        }
    }

    pub fn build(self) -> MetaClient {
        let mut client = if let Some(mgr) = self.channel_manager {
            MetaClient::with_channel_manager(self.id, mgr)
        } else {
            MetaClient::new(self.id)
        };

        if !(self.enable_heartbeat || self.enable_router || self.enable_store || self.enable_lock) {
            panic!("At least one client needs to be enabled.")
        }

        let mgr = client.channel_manager.clone();

        if self.enable_heartbeat {
            client.heartbeat = Some(HeartbeatClient::new(self.id, self.role, mgr.clone()));
        }
        if self.enable_router {
            client.router = Some(RouterClient::new(self.id, self.role, mgr.clone()));
        }
        if self.enable_store {
            client.store = Some(StoreClient::new(self.id, self.role, mgr.clone()));
        }
        if self.enable_lock {
            client.lock = Some(LockClient::new(self.id, self.role, mgr));
        }

        client
    }
}

#[derive(Clone, Debug, Default)]
pub struct MetaClient {
    id: Id,
    channel_manager: ChannelManager,
    heartbeat: Option<HeartbeatClient>,
    router: Option<RouterClient>,
    store: Option<StoreClient>,
    lock: Option<LockClient>,
}

impl MetaClient {
    pub fn new(id: Id) -> Self {
        Self {
            id,
            ..Default::default()
        }
    }

    pub fn with_channel_manager(id: Id, channel_manager: ChannelManager) -> Self {
        Self {
            id,
            channel_manager,
            ..Default::default()
        }
    }

    pub async fn start<U, A>(&mut self, urls: A) -> Result<()>
    where
        U: AsRef<str>,
        A: AsRef<[U]> + Clone,
    {
        info!("MetaClient channel config: {:?}", self.channel_config());

        if let Some(client) = &mut self.heartbeat {
            client.start(urls.clone()).await?;
            info!("Heartbeat client started");
        }
        if let Some(client) = &mut self.router {
            client.start(urls.clone()).await?;
            info!("Router client started");
        }
        if let Some(client) = &mut self.store {
            client.start(urls.clone()).await?;
            info!("Store client started");
        }

        if let Some(client) = &mut self.lock {
            client.start(urls).await?;
            info!("Lock client started");
        }

        Ok(())
    }

    /// Ask the leader address of `metasrv`, and the heartbeat component
    /// needs to create a bidirectional streaming to the leader.
    pub async fn ask_leader(&self) -> Result<()> {
        self.heartbeat_client()?.ask_leader().await
    }

    /// Returns a heartbeat bidirectional streaming: (sender, recever), the
    /// other end is the leader of `metasrv`.
    ///
    /// The `datanode` needs to use the sender to continuously send heartbeat
    /// packets (some self-state data), and the receiver can receive a response
    /// from "metasrv" (which may contain some scheduling instructions).
    pub async fn heartbeat(&self) -> Result<(HeartbeatSender, HeartbeatStream)> {
        self.heartbeat_client()?.heartbeat().await
    }

    /// Provides routing information for distributed create table requests.
    ///
    /// When a distributed create table request is received, this method returns
    /// a list of `datanode` addresses that are generated based on the partition
    /// information contained in the request and using some intelligent policies,
    /// such as load-based.
    pub async fn create_route(&self, req: CreateRequest<'_>) -> Result<RouteResponse> {
        let req = req.try_into().context(ConvertMetaRequestSnafu)?;
        self.router_client()?
            .create(req)
            .await?
            .try_into()
            .context(ConvertMetaResponseSnafu)
    }

    /// Fetch routing information for tables. The smallest unit is the complete
    /// routing information(all regions) of a table.
    ///
    /// ```text
    /// table_1
    ///    table_name
    ///    table_schema
    ///    regions
    ///      region_1
    ///        leader_peer
    ///        follower_peer_1, follower_peer_2
    ///      region_2
    ///        leader_peer
    ///        follower_peer_1, follower_peer_2, follower_peer_3
    ///      region_xxx
    /// table_2
    ///    ...
    /// ```
    ///
    pub async fn route(&self, req: RouteRequest) -> Result<RouteResponse> {
        self.router_client()?
            .route(req.into())
            .await?
            .try_into()
            .context(ConvertMetaResponseSnafu)
    }

    /// Can be called repeatedly, the first call will delete and return the
    /// table of routing information, the nth call can still return the
    /// deleted route information.
    pub async fn delete_route(&self, req: DeleteRequest) -> Result<RouteResponse> {
        self.router_client()?
            .delete(req.into())
            .await?
            .try_into()
            .context(ConvertMetaResponseSnafu)
    }

    /// Range gets the keys in the range from the key-value store.
    pub async fn range(&self, req: RangeRequest) -> Result<RangeResponse> {
        self.store_client()?
            .range(req.into())
            .await?
            .try_into()
            .context(ConvertMetaResponseSnafu)
    }

    /// Put puts the given key into the key-value store.
    pub async fn put(&self, req: PutRequest) -> Result<PutResponse> {
        self.store_client()?
            .put(req.into())
            .await?
            .try_into()
            .context(ConvertMetaResponseSnafu)
    }

    /// BatchGet atomically get values by the given keys from the key-value store.
    pub async fn batch_get(&self, req: BatchGetRequest) -> Result<BatchGetResponse> {
        self.store_client()?
            .batch_get(req.into())
            .await?
            .try_into()
            .context(ConvertMetaResponseSnafu)
    }

    /// BatchPut atomically puts the given keys into the key-value store.
    pub async fn batch_put(&self, req: BatchPutRequest) -> Result<BatchPutResponse> {
        self.store_client()?
            .batch_put(req.into())
            .await?
            .try_into()
            .context(ConvertMetaResponseSnafu)
    }

    /// BatchDelete atomically deletes the given keys from the key-value store.
    pub async fn batch_delete(&self, req: BatchDeleteRequest) -> Result<BatchDeleteResponse> {
        self.store_client()?
            .batch_delete(req.into())
            .await?
            .try_into()
            .context(ConvertMetaResponseSnafu)
    }

    /// CompareAndPut atomically puts the value to the given updated
    /// value if the current value == the expected value.
    pub async fn compare_and_put(
        &self,
        req: CompareAndPutRequest,
    ) -> Result<CompareAndPutResponse> {
        self.store_client()?
            .compare_and_put(req.into())
            .await?
            .try_into()
            .context(ConvertMetaResponseSnafu)
    }

    /// DeleteRange deletes the given range from the key-value store.
    pub async fn delete_range(&self, req: DeleteRangeRequest) -> Result<DeleteRangeResponse> {
        self.store_client()?
            .delete_range(req.into())
            .await?
            .try_into()
            .context(ConvertMetaResponseSnafu)
    }

    /// MoveValue atomically renames the key to the given updated key.
    pub async fn move_value(&self, req: MoveValueRequest) -> Result<MoveValueResponse> {
        self.store_client()?
            .move_value(req.into())
            .await?
            .try_into()
            .context(ConvertMetaResponseSnafu)
    }

    pub async fn lock(&self, req: LockRequest) -> Result<LockResponse> {
        self.lock_client()?.lock(req.into()).await.map(Into::into)
    }

    pub async fn unlock(&self, req: UnlockRequest) -> Result<()> {
        self.lock_client()?.unlock(req.into()).await?;
        Ok(())
    }

    #[inline]
    pub fn heartbeat_client(&self) -> Result<HeartbeatClient> {
        self.heartbeat.clone().context(error::NotStartedSnafu {
            name: "heartbeat_client",
        })
    }

    #[inline]
    pub fn router_client(&self) -> Result<RouterClient> {
        self.router.clone().context(error::NotStartedSnafu {
            name: "store_client",
        })
    }

    #[inline]
    pub fn store_client(&self) -> Result<StoreClient> {
        self.store.clone().context(error::NotStartedSnafu {
            name: "store_client",
        })
    }

    #[inline]
    pub fn lock_client(&self) -> Result<LockClient> {
        self.lock.clone().context(error::NotStartedSnafu {
            name: "lock_client",
        })
    }

    #[inline]
    pub fn channel_config(&self) -> &ChannelConfig {
        self.channel_manager.config()
    }

    #[inline]
    pub fn id(&self) -> Id {
        self.id
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use api::v1::meta::{HeartbeatRequest, Peer};
    use chrono::DateTime;
    use common_meta::rpc::router::Partition;
    use common_meta::table_name::TableName;
    use datatypes::prelude::ConcreteDataType;
    use datatypes::schema::{ColumnSchema, RawSchema};
    use meta_srv::metasrv::SelectorContext;
    use meta_srv::selector::{Namespace, Selector};
    use meta_srv::Result as MetaResult;
    use table::metadata::{RawTableInfo, RawTableMeta, TableIdent, TableType};
    use table::requests::TableOptions;

    use super::*;
    use crate::mocks;

    const TEST_KEY_PREFIX: &str = "__unit_test__meta__";

    struct TestClient {
        ns: String,
        client: MetaClient,
    }

    impl TestClient {
        async fn new(ns: impl Into<String>) -> Self {
            // can also test with etcd: mocks::mock_client_with_etcdstore("127.0.0.1:2379").await;
            let client = mocks::mock_client_with_memstore().await;
            Self {
                ns: ns.into(),
                client,
            }
        }

        fn key(&self, name: &str) -> Vec<u8> {
            format!("{}-{}-{}", TEST_KEY_PREFIX, self.ns, name).into_bytes()
        }

        async fn gen_data(&self) {
            for i in 0..10 {
                let req = PutRequest::new()
                    .with_key(self.key(&format!("key-{i}")))
                    .with_value(format!("{}-{}", "value", i).into_bytes())
                    .with_prev_kv();
                let res = self.client.put(req).await;
                assert!(res.is_ok());
            }
        }

        async fn clear_data(&self) {
            let req =
                DeleteRangeRequest::new().with_prefix(format!("{}-{}", TEST_KEY_PREFIX, self.ns));
            let res = self.client.delete_range(req).await;
            assert!(res.is_ok());
        }
    }

    async fn new_client(ns: impl Into<String>) -> TestClient {
        let client = TestClient::new(ns).await;
        client.clear_data().await;
        client
    }

    #[tokio::test]
    async fn test_meta_client_builder() {
        let urls = &["127.0.0.1:3001", "127.0.0.1:3002"];

        let mut meta_client = MetaClientBuilder::new(0, 0, Role::Datanode)
            .enable_heartbeat()
            .build();
        assert!(meta_client.heartbeat_client().is_ok());
        assert!(meta_client.router_client().is_err());
        assert!(meta_client.store_client().is_err());
        meta_client.start(urls).await.unwrap();
        assert!(meta_client.heartbeat_client().unwrap().is_started().await);

        let mut meta_client = MetaClientBuilder::new(0, 0, Role::Datanode)
            .enable_router()
            .build();
        assert!(meta_client.heartbeat_client().is_err());
        assert!(meta_client.router_client().is_ok());
        assert!(meta_client.store_client().is_err());
        meta_client.start(urls).await.unwrap();
        assert!(meta_client.router_client().unwrap().is_started().await);

        let mut meta_client = MetaClientBuilder::new(0, 0, Role::Datanode)
            .enable_store()
            .build();
        assert!(meta_client.heartbeat_client().is_err());
        assert!(meta_client.router_client().is_err());
        assert!(meta_client.store_client().is_ok());
        meta_client.start(urls).await.unwrap();
        assert!(meta_client.store_client().unwrap().is_started().await);

        let mut meta_client = MetaClientBuilder::new(1, 2, Role::Datanode)
            .enable_heartbeat()
            .enable_router()
            .enable_store()
            .build();
        assert_eq!(1, meta_client.id().0);
        assert_eq!(2, meta_client.id().1);
        assert!(meta_client.heartbeat_client().is_ok());
        assert!(meta_client.router_client().is_ok());
        assert!(meta_client.store_client().is_ok());
        meta_client.start(urls).await.unwrap();
        assert!(meta_client.heartbeat_client().unwrap().is_started().await);
        assert!(meta_client.router_client().unwrap().is_started().await);
        assert!(meta_client.store_client().unwrap().is_started().await);
    }

    #[tokio::test]
    async fn test_not_start_heartbeat_client() {
        let urls = &["127.0.0.1:3001", "127.0.0.1:3002"];
        let mut meta_client = MetaClientBuilder::new(0, 0, Role::Datanode)
            .enable_router()
            .enable_store()
            .build();
        meta_client.start(urls).await.unwrap();
        let res = meta_client.ask_leader().await;
        assert!(matches!(res.err(), Some(error::Error::NotStarted { .. })));
    }

    fn new_table_info() -> RawTableInfo {
        RawTableInfo {
            ident: TableIdent {
                table_id: 0,
                version: 0,
            },
            name: "t".to_string(),
            desc: None,
            catalog_name: "c".to_string(),
            schema_name: "s".to_string(),
            meta: RawTableMeta {
                schema: RawSchema {
                    column_schemas: vec![ColumnSchema::new(
                        "ts",
                        ConcreteDataType::timestamp_millisecond_datatype(),
                        false,
                    )],
                    timestamp_index: Some(0),
                    version: 0,
                },
                primary_key_indices: vec![],
                value_indices: vec![],
                engine: "mito".to_string(),
                next_column_id: 0,
                region_numbers: vec![],
                engine_options: HashMap::new(),
                options: TableOptions::default(),
                created_on: DateTime::default(),
            },
            table_type: TableType::Base,
        }
    }

    #[tokio::test]
    async fn test_not_start_router_client() {
        let urls = &["127.0.0.1:3001", "127.0.0.1:3002"];
        let mut meta_client = MetaClientBuilder::new(0, 0, Role::Datanode)
            .enable_heartbeat()
            .enable_store()
            .build();
        meta_client.start(urls).await.unwrap();

        let table_info = new_table_info();
        let req = CreateRequest::new(TableName::new("c", "s", "t"), &table_info);
        let res = meta_client.create_route(req).await;
        assert!(matches!(res.err(), Some(error::Error::NotStarted { .. })));
    }

    #[tokio::test]
    async fn test_not_start_store_client() {
        let urls = &["127.0.0.1:3001", "127.0.0.1:3002"];
        let mut meta_client = MetaClientBuilder::new(0, 0, Role::Datanode)
            .enable_heartbeat()
            .enable_router()
            .build();

        meta_client.start(urls).await.unwrap();
        let res = meta_client.put(PutRequest::default()).await;
        assert!(matches!(res.err(), Some(error::Error::NotStarted { .. })));
    }

    #[should_panic]
    #[test]
    fn test_failed_when_start_nothing() {
        let _ = MetaClientBuilder::new(0, 0, Role::Datanode).build();
    }

    #[tokio::test]
    async fn test_ask_leader() {
        let tc = new_client("test_ask_leader").await;
        let res = tc.client.ask_leader().await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_heartbeat() {
        let tc = new_client("test_heartbeat").await;
        let (sender, mut receiver) = tc.client.heartbeat().await.unwrap();
        // send heartbeats
        tokio::spawn(async move {
            for _ in 0..5 {
                let req = HeartbeatRequest {
                    peer: Some(Peer {
                        id: 1,
                        addr: "meta_client_peer".to_string(),
                    }),
                    ..Default::default()
                };
                sender.send(req).await.unwrap();
            }
        });

        tokio::spawn(async move {
            while let Some(res) = receiver.message().await.unwrap() {
                assert_eq!(1000, res.header.unwrap().cluster_id);
            }
        });
    }

    struct MockSelector;

    #[async_trait::async_trait]
    impl Selector for MockSelector {
        type Context = SelectorContext;
        type Output = Vec<Peer>;

        async fn select(&self, _ns: Namespace, _ctx: &Self::Context) -> MetaResult<Self::Output> {
            Ok(vec![
                Peer {
                    id: 0,
                    addr: "peer0".to_string(),
                },
                Peer {
                    id: 1,
                    addr: "peer1".to_string(),
                },
                Peer {
                    id: 2,
                    addr: "peer2".to_string(),
                },
            ])
        }
    }

    #[tokio::test]
    async fn test_route() {
        let selector = Arc::new(MockSelector {});
        let client = mocks::mock_client_with_memorystore_and_selector(selector).await;

        let p1 = Partition {
            column_list: vec![b"col_1".to_vec(), b"col_2".to_vec()],
            value_list: vec![b"k1".to_vec(), b"k2".to_vec()],
        };
        let p2 = Partition {
            column_list: vec![b"col_1".to_vec(), b"col_2".to_vec()],
            value_list: vec![b"Max1".to_vec(), b"Max2".to_vec()],
        };
        let table_name = TableName::new("test_catalog", "test_schema", "test_table");
        let table_info = new_table_info();
        let req = CreateRequest::new(table_name.clone(), &table_info)
            .add_partition(p1)
            .add_partition(p2);

        let res = client.create_route(req).await.unwrap();
        assert_eq!(1, res.table_routes.len());

        let req = RouteRequest::new().add_table_name(table_name.clone());
        let res = client.route(req).await.unwrap();
        assert!(!res.table_routes.is_empty());

        let req = DeleteRequest::new(table_name.clone());
        let res = client.delete_route(req).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_range_get() {
        let tc = new_client("test_range_get").await;
        tc.gen_data().await;

        let key = tc.key("key-0");
        let req = RangeRequest::new().with_key(key.as_slice());
        let res = tc.client.range(req).await;
        let mut kvs = res.unwrap().take_kvs();
        assert_eq!(1, kvs.len());
        let mut kv = kvs.pop().unwrap();
        assert_eq!(key, kv.take_key());
        assert_eq!(b"value-0".to_vec(), kv.take_value());
    }

    #[tokio::test]
    async fn test_range_get_prefix() {
        let tc = new_client("test_range_get_prefix").await;
        tc.gen_data().await;

        let req = RangeRequest::new().with_prefix(tc.key("key-"));
        let res = tc.client.range(req).await;
        let kvs = res.unwrap().take_kvs();
        assert_eq!(10, kvs.len());
        for (i, mut kv) in kvs.into_iter().enumerate() {
            assert_eq!(tc.key(&format!("key-{i}")), kv.take_key());
            assert_eq!(format!("{}-{}", "value", i).into_bytes(), kv.take_value());
        }
    }

    #[tokio::test]
    async fn test_range() {
        let tc = new_client("test_range").await;
        tc.gen_data().await;

        let req = RangeRequest::new().with_range(tc.key("key-5"), tc.key("key-8"));
        let res = tc.client.range(req).await;
        let kvs = res.unwrap().take_kvs();
        assert_eq!(3, kvs.len());
        for (i, mut kv) in kvs.into_iter().enumerate() {
            assert_eq!(tc.key(&format!("key-{}", i + 5)), kv.take_key());
            assert_eq!(
                format!("{}-{}", "value", i + 5).into_bytes(),
                kv.take_value()
            );
        }
    }

    #[tokio::test]
    async fn test_range_keys_only() {
        let tc = new_client("test_range_keys_only").await;
        tc.gen_data().await;

        let req = RangeRequest::new()
            .with_range(tc.key("key-5"), tc.key("key-8"))
            .with_keys_only();
        let res = tc.client.range(req).await;
        let kvs = res.unwrap().take_kvs();
        assert_eq!(3, kvs.len());
        for (i, mut kv) in kvs.into_iter().enumerate() {
            assert_eq!(tc.key(&format!("key-{}", i + 5)), kv.take_key());
            assert!(kv.take_value().is_empty());
        }
    }

    #[tokio::test]
    async fn test_put() {
        let tc = new_client("test_put").await;

        let req = PutRequest::new()
            .with_key(tc.key("key"))
            .with_value(b"value".to_vec());
        let res = tc.client.put(req).await;
        assert!(res.unwrap().take_prev_kv().is_none());
    }

    #[tokio::test]
    async fn test_put_with_prev_kv() {
        let tc = new_client("test_put_with_prev_kv").await;

        let key = tc.key("key");
        let req = PutRequest::new()
            .with_key(key.as_slice())
            .with_value(b"value".to_vec())
            .with_prev_kv();
        let res = tc.client.put(req).await;
        assert!(res.unwrap().take_prev_kv().is_none());

        let req = PutRequest::new()
            .with_key(key.as_slice())
            .with_value(b"value1".to_vec())
            .with_prev_kv();
        let res = tc.client.put(req).await;
        let mut kv = res.unwrap().take_prev_kv().unwrap();
        assert_eq!(key, kv.take_key());
        assert_eq!(b"value".to_vec(), kv.take_value());
    }

    #[tokio::test]
    async fn test_batch_put() {
        let tc = new_client("test_batch_put").await;

        let mut req = BatchPutRequest::new();
        for i in 0..275 {
            req = req.add_kv(
                tc.key(&format!("key-{}", i)),
                format!("value-{}", i).into_bytes(),
            );
        }

        let res = tc.client.batch_put(req).await;
        assert_eq!(0, res.unwrap().take_prev_kvs().len());

        let req = RangeRequest::new().with_prefix(tc.key("key-"));
        let res = tc.client.range(req).await;
        let kvs = res.unwrap().take_kvs();
        assert_eq!(275, kvs.len());
    }

    #[tokio::test]
    async fn test_batch_get() {
        let tc = new_client("test_batch_get").await;
        tc.gen_data().await;

        let mut req = BatchGetRequest::default();
        for i in 0..256 {
            req = req.add_key(tc.key(&format!("key-{}", i)));
        }
        let mut res = tc.client.batch_get(req).await.unwrap();

        assert_eq!(10, res.take_kvs().len());

        let req = BatchGetRequest::default()
            .add_key(tc.key("key-1"))
            .add_key(tc.key("key-999"));
        let mut res = tc.client.batch_get(req).await.unwrap();

        assert_eq!(1, res.take_kvs().len());
    }

    #[tokio::test]
    async fn test_batch_put_with_prev_kv() {
        let tc = new_client("test_batch_put_with_prev_kv").await;

        let key = tc.key("key");
        let key2 = tc.key("key2");
        let req = BatchPutRequest::new().add_kv(key.as_slice(), b"value".to_vec());
        let res = tc.client.batch_put(req).await;
        assert_eq!(0, res.unwrap().take_prev_kvs().len());

        let req = BatchPutRequest::new()
            .add_kv(key.as_slice(), b"value-".to_vec())
            .add_kv(key2.as_slice(), b"value2-".to_vec())
            .with_prev_kv();
        let res = tc.client.batch_put(req).await;
        let mut kvs = res.unwrap().take_prev_kvs();
        assert_eq!(1, kvs.len());
        let mut kv = kvs.pop().unwrap();
        assert_eq!(key, kv.take_key());
        assert_eq!(b"value".to_vec(), kv.take_value());
    }

    #[tokio::test]
    async fn test_compare_and_put() {
        let tc = new_client("test_compare_and_put").await;

        let key = tc.key("key");
        let req = CompareAndPutRequest::new()
            .with_key(key.as_slice())
            .with_expect(b"expect".to_vec())
            .with_value(b"value".to_vec());
        let res = tc.client.compare_and_put(req).await;
        assert!(!res.unwrap().is_success());

        // create if absent
        let req = CompareAndPutRequest::new()
            .with_key(key.as_slice())
            .with_value(b"value".to_vec());
        let res = tc.client.compare_and_put(req).await;
        let mut res = res.unwrap();
        assert!(res.is_success());
        assert!(res.take_prev_kv().is_none());

        // compare and put fail
        let req = CompareAndPutRequest::new()
            .with_key(key.as_slice())
            .with_expect(b"not_eq".to_vec())
            .with_value(b"value2".to_vec());
        let res = tc.client.compare_and_put(req).await;
        let mut res = res.unwrap();
        assert!(!res.is_success());
        assert_eq!(b"value".to_vec(), res.take_prev_kv().unwrap().take_value());

        // compare and put success
        let req = CompareAndPutRequest::new()
            .with_key(key.as_slice())
            .with_expect(b"value".to_vec())
            .with_value(b"value2".to_vec());
        let res = tc.client.compare_and_put(req).await;
        let mut res = res.unwrap();
        assert!(res.is_success());
        assert_eq!(b"value".to_vec(), res.take_prev_kv().unwrap().take_value());
    }

    #[tokio::test]
    async fn test_delete_with_key() {
        let tc = new_client("test_delete_with_key").await;
        tc.gen_data().await;

        let req = DeleteRangeRequest::new()
            .with_key(tc.key("key-0"))
            .with_prev_kv();
        let res = tc.client.delete_range(req).await;
        let mut res = res.unwrap();
        assert_eq!(1, res.deleted());
        let mut kvs = res.take_prev_kvs();
        assert_eq!(1, kvs.len());
        let mut kv = kvs.pop().unwrap();
        assert_eq!(b"value-0".to_vec(), kv.take_value());
    }

    #[tokio::test]
    async fn test_delete_with_prefix() {
        let tc = new_client("test_delete_with_prefix").await;
        tc.gen_data().await;

        let req = DeleteRangeRequest::new()
            .with_prefix(tc.key("key-"))
            .with_prev_kv();
        let res = tc.client.delete_range(req).await;
        let mut res = res.unwrap();
        assert_eq!(10, res.deleted());
        let kvs = res.take_prev_kvs();
        assert_eq!(10, kvs.len());
        for (i, mut kv) in kvs.into_iter().enumerate() {
            assert_eq!(format!("{}-{}", "value", i).into_bytes(), kv.take_value());
        }
    }

    #[tokio::test]
    async fn test_delete_with_range() {
        let tc = new_client("test_delete_with_range").await;
        tc.gen_data().await;

        let req = DeleteRangeRequest::new()
            .with_range(tc.key("key-2"), tc.key("key-7"))
            .with_prev_kv();
        let res = tc.client.delete_range(req).await;
        let mut res = res.unwrap();
        assert_eq!(5, res.deleted());
        let kvs = res.take_prev_kvs();
        assert_eq!(5, kvs.len());
        for (i, mut kv) in kvs.into_iter().enumerate() {
            assert_eq!(
                format!("{}-{}", "value", i + 2).into_bytes(),
                kv.take_value()
            );
        }
    }

    #[tokio::test]
    async fn test_move_value() {
        let tc = new_client("test_move_value").await;

        let from_key = tc.key("from_key");
        let to_key = tc.key("to_key");

        let req = MoveValueRequest::new(from_key.as_slice(), to_key.as_slice());
        let res = tc.client.move_value(req).await;
        assert!(res.unwrap().take_kv().is_none());

        let req = PutRequest::new()
            .with_key(to_key.as_slice())
            .with_value(b"value".to_vec());
        let _ = tc.client.put(req).await;

        let req = MoveValueRequest::new(from_key.as_slice(), to_key.as_slice());
        let res = tc.client.move_value(req).await;
        let mut kv = res.unwrap().take_kv().unwrap();
        assert_eq!(to_key.clone(), kv.take_key());
        assert_eq!(b"value".to_vec(), kv.take_value());

        let req = PutRequest::new()
            .with_key(from_key.as_slice())
            .with_value(b"value2".to_vec());
        let _ = tc.client.put(req).await;

        let req = MoveValueRequest::new(from_key.as_slice(), to_key.as_slice());
        let res = tc.client.move_value(req).await;
        let mut kv = res.unwrap().take_kv().unwrap();
        assert_eq!(from_key, kv.take_key());
        assert_eq!(b"value2".to_vec(), kv.take_value());
    }
}
