use librarius::{
    FileSource, Librarius, LibrariusBuilder, MemorySource, ObjectSize, Persistent,
    PersistentPointer, Result, TypedLibrariusBuilder, TypedTransaction,
};
use std::env;

struct Data {
    value: usize,
}

impl Persistent for Data {
    fn size() -> ObjectSize {
        ObjectSize::new_with_usize(0, std::mem::size_of::<Data>())
    }
}

struct Root {
    data: PersistentPointer<Data>,
    value: usize,
}

impl Persistent for Root {
    fn size() -> ObjectSize {
        ObjectSize::new_with_usize(8, 8)
    }
}

impl Root {
    fn new() -> Root {
        println!("running constructor...");
        Root {
            data: PersistentPointer::new_none(),
            value: 5,
        }
    }
}

fn main() -> Result<()> {
    let file = env::args()
        .nth(1)
        .expect("no database source file specified");

    let librarius = LibrariusBuilder::new()
        .create_with_typed(|| Root::new())
        .source(MemorySource::new(1 << 20)?)
        .source(FileSource::new(file.as_str(), 1 << 20)?)
        .open()?;

    let value = librarius.run(|tx| {
        let root = tx.root_typed::<Root>();
        let rootp = tx.read_typed(root)?;

        Ok(rootp.value)
    })?;

    println!("{}", value);

    Ok(())
}
