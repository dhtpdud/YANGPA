//! `note.*`: 다른 사용자에 대한 개인 메모. 내 기기끼리만 봉인해 맞춥니다(`notes`).

use std::sync::Arc;

use yangpa_rpc::api::{NoteView, m};

use super::{CallCtx, P, R, me, rpc};
use crate::node::Node;
use crate::notes;

pub(crate) async fn get(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::NoteGet>) -> R<m::NoteGet> {
    me(node)?;
    Ok(NoteView {
        text: rpc(notes::get(node, &p.user))?,
    })
}

pub(crate) async fn set(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::NoteSet>) -> R<m::NoteSet> {
    rpc(notes::set(node, &p.user, &p.text))?;
    Ok(super::empty())
}
