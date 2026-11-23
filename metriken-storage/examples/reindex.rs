//! Rewrite a dendro archive's names indexes in place
//! ([`metriken_storage::reindex::reindex`]) and print what it did and how
//! long it took.
//!
//! ```sh
//! cargo run --release -p metriken-storage --example reindex --features write -- ARCHIVE
//! ```

use std::path::Path;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: reindex ARCHIVE")?;
    let start = Instant::now();
    let report = metriken_storage::reindex::reindex(Path::new(&path))?;
    println!("{report:?} in {:.2} s", start.elapsed().as_secs_f64());
    Ok(())
}
