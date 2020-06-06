use librarius::{Librarius, Result, MemorySource, FileSource};

use std::env;

fn main() -> Result<()> {
    let file = env::args()
        .nth(1)
        .expect("no database source file specified");

    let mut librarius = Librarius::new(4096);

    librarius.attach(MemorySource::new(1 << 20)?)?;
    librarius.attach(FileSource::new(file.as_str(), 1 << 20)?)?;

    librarius.root_alloc_if_none(1024, |buf| {
        buf[1] = 5;
        Ok(())
    })?;

    let value = librarius.run(|tx| {
        let root = tx.root()?;
        let root_data = tx.read(root, 1024)?;

        Ok(root_data[1])
    })?;

    println!("{}", value);

    Ok(())
}
