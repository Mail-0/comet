use zeron_proto::{CLOUD_ACCOUNTS_DEVICE, HarnessId};
use zeron_rpc::{RpcError, RpcReply};

use crate::{agent_accounts::AgentAccounts, cloud_client::CloudClient};

pub(crate) fn handles(_method: &str) -> bool {
    false
}

pub(crate) async fn dispatch(
    _cloud: CloudClient,
    _accounts: AgentAccounts,
    method: &str,
    _params: serde_json::Value,
) -> Result<RpcReply, RpcError> {
    Err(RpcError::UnknownMethod(method.into()))
}

pub(crate) async fn ready_harnesses(_cloud: &CloudClient) -> Option<Vec<HarnessId>> {
    Some(Vec::new())
}

pub(crate) fn is_cloud_accounts_target(target: &str) -> bool {
    target == CLOUD_ACCOUNTS_DEVICE
}
