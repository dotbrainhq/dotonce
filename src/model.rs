use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct TurnIn {
    pub speaker: String,
    pub text: String,
}

/// One conversation session as the adapter sends it.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionDoc {
    pub id: String,
    pub user_id: Option<String>,
    pub timestamp: Option<String>,
    pub turns: Vec<TurnIn>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SessionHit {
    pub id: String,
    pub timestamp: Option<String>,
    pub text: String,
    pub score: f64,
}
