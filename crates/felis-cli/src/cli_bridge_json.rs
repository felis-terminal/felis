//! Frame bodies the bridge renders as JSON, under the CLI's own `v: 1`
//! machine-output contract (`docs/reference/cli.md` "Machine output").
//! A separate contract from `felis-grid`'s `felis-json` format, which
//! covers the families a bridge client never sees.

use anyhow::bail;
use felis_protocol::messages::{
    NotifyToClientMsg, OpsToClientMsg, PushMsg, RegionToClientMsg, SearchToClientMsg,
};
use felis_protocol::{MessageKind, codec};
use serde_json::Value;

pub(crate) fn body_to_json(kind: MessageKind, body: &[u8]) -> anyhow::Result<Value> {
    Ok(match kind {
        MessageKind::Ops => serde_json::to_value(codec::decode::<OpsToClientMsg>(body)?)?,
        MessageKind::Region => serde_json::to_value(codec::decode::<RegionToClientMsg>(body)?)?,
        MessageKind::Notify => serde_json::to_value(codec::decode::<NotifyToClientMsg>(body)?)?,
        MessageKind::Push => serde_json::to_value(codec::decode::<PushMsg>(body)?)?,
        MessageKind::Search => serde_json::to_value(codec::decode::<SearchToClientMsg>(body)?)?,
        other => bail!("the bridge renders no {other:?} frame as JSON"),
    })
}
