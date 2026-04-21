use std::io::IsTerminal;

pub enum Stream {
    Stdout,
    Stderr,
    Stdin,
}

pub fn is(stream: Stream) -> bool {
    match stream {
        Stream::Stdout => std::io::stdout().is_terminal(),
        Stream::Stderr => std::io::stderr().is_terminal(),
        Stream::Stdin => std::io::stdin().is_terminal(),
    }
}
