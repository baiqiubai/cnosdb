use std::fmt::{Debug, Display, Formatter, Result};

use meta::model::{MetaClientRef, MetaRef};
use models::consistency_level::ConsistencyLevel;
use protos::kv_service::{AdminCommandRequest, WritePointsRequest};
use serde::{Deserialize, Serialize};
use tskv::query_iterator::QueryOption;
use tskv::EngineRef;

use crate::errors::CoordinatorResult;
use crate::reader::ReaderIterator;

pub mod errors;
pub mod file_info;
pub mod hh_queue;
pub mod metrics;
pub mod node_mgr;
pub mod reader;
pub mod service;
pub mod service_mock;
pub mod vnode_mgr;
pub mod writer;

pub const FAILED_RESPONSE_CODE: i32 = -1;
pub const FINISH_RESPONSE_CODE: i32 = 0;
pub const SUCCESS_RESPONSE_CODE: i32 = 1;

#[derive(Debug)]
pub struct WriteRequest {
    pub tenant: String,
    pub level: models::consistency_level::ConsistencyLevel,
    pub request: protos::kv_service::WritePointsRequest,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Default, Clone)]
pub enum NodeState {
    #[default]
    Running,
    Pending,
    Cold,
    Unknown,
}

impl Display for NodeState {
    fn fmt(&self, f: &mut Formatter) -> Result {
        match self {
            NodeState::Running => write!(f, "Running"),
            NodeState::Pending => write!(f, "Pending"),
            NodeState::Cold => write!(f, "Cold"),
            NodeState::Unknown => write!(f, "Unknown state"),
        }
    }
}

impl From<String> for NodeState {
    fn from(node_state: String) -> Self {
        match node_state.as_str() {
            "Running" => NodeState::Running,
            "Pending" => NodeState::Pending,
            "Cold" => NodeState::Cold,
            _ => NodeState::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub enum NodeManagerCmdType {
    ChangeNodeState(u64, NodeState),
}

#[derive(Debug, Clone)]
pub enum VnodeManagerCmdType {
    // vnode id, dst node id
    Copy(u32, u64),
    // vnode id, dst node id
    Move(u32, u64),
    // vnode id
    Drop(u32),
    // vnode is list
    Compact(Vec<u32>),
}

pub fn status_response_to_result(
    status: &protos::kv_service::StatusResponse,
) -> errors::CoordinatorResult<()> {
    if status.code == SUCCESS_RESPONSE_CODE {
        Ok(())
    } else {
        Err(errors::CoordinatorError::GRPCRequest {
            msg: format!("server status: {}, {}", status.code, status.data),
        })
    }
}

#[async_trait::async_trait]
pub trait Coordinator: Send + Sync + Debug {
    fn node_id(&self) -> u64;
    fn meta_manager(&self) -> MetaRef;
    fn store_engine(&self) -> Option<EngineRef>;
    async fn tenant_meta(&self, tenant: &str) -> Option<MetaClientRef>;

    async fn write_points(
        &self,
        tenant: String,
        level: ConsistencyLevel,
        request: WritePointsRequest,
    ) -> CoordinatorResult<()>;

    fn read_record(&self, option: QueryOption) -> CoordinatorResult<ReaderIterator>;

    fn tag_scan(&self, option: QueryOption) -> CoordinatorResult<ReaderIterator>;

    async fn broadcast_command(&self, req: AdminCommandRequest) -> CoordinatorResult<()>;

    async fn vnode_manager(
        &self,
        tenant: &str,
        cmd_type: VnodeManagerCmdType,
    ) -> CoordinatorResult<()>;

    async fn node_manager(
        &self,
        tenant: &str,
        cmd_type: NodeManagerCmdType,
    ) -> CoordinatorResult<()>;
}
