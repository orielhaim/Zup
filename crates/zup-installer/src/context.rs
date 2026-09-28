//! What this process is, as far as the runtime is concerned.

use zup_core::Frontend;
use zup_presentation::OutputFormat;

/// The identity and the audience of one runtime process.
///
/// The binary already knows which frontend it was compiled for, and the parser
/// already knows which output format was asked for. Both are values here rather
/// than process-wide state, so every command receives the answer as an argument
/// and no command has to ask the process what it is.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeContext {
    /// The presentation this binary was built as.
    pub frontend: Frontend,
    /// The format machine output is written in, or `Human`.
    pub output: OutputFormat,
}

impl RuntimeContext {
    /// The context for a process that has not yet been told an output format.
    pub fn new(frontend: Frontend) -> Self {
        Self {
            frontend,
            output: OutputFormat::Human,
        }
    }

    /// The same context, writing machine output in `output`.
    pub fn with_output(mut self, output: OutputFormat) -> Self {
        self.output = output;
        self
    }

    /// Whether this context is the one an interactive console install uses.
    ///
    /// A console build reached through a redirected stream is still the console
    /// frontend, but it is answering a script rather than a person, and it must
    /// not block waiting for an answer that is not coming.
    pub fn is_live_console(self) -> bool {
        self.frontend == Frontend::Console && console_is_a_terminal()
    }
}

/// Whether the standard streams are a terminal a person is sitting at.
pub fn console_is_a_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}
