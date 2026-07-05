#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Workbench,
}

#[derive(Debug, Clone)]
pub enum Message {
    NoOp,
}
