use std::collections::{HashMap, VecDeque};

pub const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 512 * 1024;
pub const MAX_PENDING: usize = 8;
pub const MAX_CONCURRENT: usize = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtRequest {
    pub request_id: String,
    pub zone_handle: String,
    pub art_capability: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtResponse {
    pub request_id: String,
    pub art_revision: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ArtError {
    #[error("artwork lane is full")]
    Busy,
    #[error("artwork response exceeds bound")]
    TooLarge,
    #[error("artwork capability or id is invalid")]
    InvalidRequest,
}

#[derive(Default)]
pub struct ArtLane {
    pending: VecDeque<ArtRequest>,
    active: usize,
    completed: HashMap<String, ArtResponse>,
    seen_capabilities: HashMap<String, String>,
}
impl ArtLane {
    pub fn enqueue(&mut self, request: ArtRequest) -> Result<(), ArtError> {
        if request.request_id.len() < 16
            || request.zone_handle.len() < 16
            || request.art_capability.len() < 32
            || request.art_capability.contains("://")
            || request.art_capability.contains('/')
            || request.art_capability.contains('?')
            || request.art_capability.contains('#')
        {
            return Err(ArtError::InvalidRequest);
        }
        if let Some(previous_request) = self.seen_capabilities.get(&request.art_capability) {
            if previous_request != &request.request_id {
                return Err(ArtError::InvalidRequest);
            }
        } else {
            self.seen_capabilities
                .insert(request.art_capability.clone(), request.request_id.clone());
        }
        if self.pending.len() >= MAX_PENDING {
            return Err(ArtError::Busy);
        }
        self.pending.push_back(request);
        Ok(())
    }
    pub fn start_next(&mut self) -> Option<ArtRequest> {
        if self.active >= MAX_CONCURRENT {
            None
        } else {
            self.pending.pop_front().inspect(|_| self.active += 1)
        }
    }
    pub fn finish(&mut self, response: ArtResponse) -> Result<(), ArtError> {
        if response.bytes.len() > MAX_OUTPUT_BYTES {
            // A rejected source still completed its slot.  Releasing it here
            // prevents one hostile/oversized response from permanently
            // exhausting the bounded lane.
            self.active = self.active.saturating_sub(1);
            return Err(ArtError::TooLarge);
        }
        self.active = self.active.saturating_sub(1);
        self.completed.insert(response.request_id.clone(), response);
        Ok(())
    }
    pub fn result(&self, request_id: &str) -> Option<&ArtResponse> {
        self.completed.get(request_id)
    }
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}
