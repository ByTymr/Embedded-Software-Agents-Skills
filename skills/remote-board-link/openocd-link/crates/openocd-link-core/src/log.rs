use std::sync::OnceLock;

static SINK: OnceLock<fn(&str)> = OnceLock::new();

/// Uygulamanın günlüğünü bağlar; bağlanmazsa satırlar atılır (testler dahil).
pub fn set_logger(sink: fn(&str)) {
    let _ = SINK.set(sink);
}

pub fn log(line: impl AsRef<str>) {
    if let Some(sink) = SINK.get() {
        sink(line.as_ref());
    }
}
