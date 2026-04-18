use rpc::{RpcReply, data::frontend_to_worker::Data as FrontendRequest};

#[derive(Debug)]
pub struct GetStatusEvent {
    pub reply: RpcReply,
}

#[derive(Debug)]
pub struct SubscribeAttachedEvent {
    pub reply: RpcReply,
}

#[derive(Debug)]
pub struct SubmitInputEvent {
    pub reply: RpcReply,
    pub manager_session_id: Option<String>,
    pub text: String,
}

#[derive(Debug)]
pub struct StopRunEvent {
    pub reply: RpcReply,
}

#[derive(Debug)]
pub enum FrontendEvent {
    GetStatus(GetStatusEvent),
    SubscribeAttached(SubscribeAttachedEvent),
    SubmitInput(SubmitInputEvent),
    StopRun(StopRunEvent),
}

impl FrontendEvent {
    pub fn from_request(value: FrontendRequest, reply: RpcReply) -> Self {
        match value {
            FrontendRequest::GetStatus => Self::GetStatus(GetStatusEvent { reply }),
            FrontendRequest::SubscribeAttached => {
                Self::SubscribeAttached(SubscribeAttachedEvent { reply })
            }
            FrontendRequest::SubmitInput {
                manager_session_id,
                text,
            } => Self::SubmitInput(SubmitInputEvent {
                reply,
                manager_session_id,
                text,
            }),
            FrontendRequest::StopRun => Self::StopRun(StopRunEvent { reply }),
        }
    }
}
