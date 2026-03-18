#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    Done,
    Error(String),
}
