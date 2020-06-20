use librarius::{FileSource, Librarius, LibrariusBuilder, MemorySource, Result};

use std::env;

fn main() -> Result<()> {
    let file = env::args()
        .nth(1)
        .expect("no database source file specified");

    let librarius = LibrariusBuilder::new()
        .create_with(1024, |data| {
            data[1] = 5;
            Ok(())
        })
        .source(MemorySource::new(1 << 20)?)
        .source(FileSource::new(file.as_str(), 1 << 20)?)
        .open()?;

    let value = librarius.run(|tx| {
        let root = tx.root()?;
        let root_data = tx.read(root, 1024)?;

        Ok(root_data[1])
    })?;

    println!("{}", value);

    Ok(())
}
