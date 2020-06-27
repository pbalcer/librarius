![Rust](https://github.com/pbalcer/librarius/workflows/Rust/badge.svg?branch=master)

# **librarius: Multi-tier MVOCC storage engine (WIP)**

*Here be dragons.*

## Goals

 - Compile-time race- and deadlock- freedom
 - Zero-cost abstraction for ACID transactional operations
    - (Near) zero-cost reads
    - Copy-on-write for easy coding
    - Interior mutability for easy scalability
 - In-memory performance for in-memory workloads with graceful degradation
 across tiers
 - No or minimal configuration knobs
 - Asynchronous replication with consistent
 (but possibly stale) replica reads
 - Efficient utilization of contemporary hardware
     - Heterogenous memory (Persistent Memory and HBM)
     - NUMA systems
     - SSDs using modern async I/O interfaces
     - RDMA networking
 - Minimally unsafe, simple, easy to read and tested codebase

## Anti-goals

 - Support for HDDs and other hardware with high seek latency
 - Distributed consensus
 - Multiprocess support

## Design ideas

Background information and glossary in [14].

 - Separation of transactional system (librarius) from the
 indexing data structure (omnis)
   - omnis will be a separate crate
 - MVOCC concurrency protocol with serializable snapshot isolation
 - Append-only version storage, with new to old ordering of entries
 - Delta storage for interior mutability atomics
 - Transaction-level garbage collection with eager version
 pruning
 - DMA-accelerated asynchronous compaction [15]
 - Physical swizzled pointers
 - Trie-based index with variably sized nodes
 - Optimistically optimistic transactions using Hardware Transactional Memory
 - Fragmentation-aware replacement policy
 - Cross-page buddy allocation scheme for objects

## Done(ish)

 - "Scalable" transactions interface
 - Source management & page allocation
 - Persistent box lifetime management
 - Basic pagetable
 - Optional secondary persistent data source for wholly in-memory database
 - basic MVCC/STM implementation
 - pagetable-less design
 - partial unsafe type support

## Todo

 - Derive macro for Persistent trait, automatic type conflict detection & lazy upgrade
 - Page eviction (first random, than maybe CAR [1]) [7, 11]
 - logical address space for pagetable, pointer swizzling [5, 8]
 - Transaction sequence & logging [6, 14]
 - Interval-based transactional reclamation [3]
 - Page compaction
 - Transaction recovery protocol
 - Snapshot/View public interface (Transaction impl Snapshot)
 - Interior mutability atomics [4, 7 - mini pages]
   - Merge operators
 - Asynchronous transaction commit (incl., I/O worker thread(s))
 - io-uring file source [2]
 - pmem2-rs and pmem source implementation
 - Streaming replication
 - Index (omnis) interface + trie implementation [9, 10, 12, 13]
 - Index async prefix subscribers
 - Range scans will probably be slow... Compaction should take sequential order on index into consideration (how?).
 - Recording source and consistency tests

### References

[1] - https://www.usenix.org/legacy/publications/library/proceedings/fast04/tech/full_papers/bansal/bansal.pdf

[2] - https://unixism.net/loti/

[3] - https://15721.courses.cs.cmu.edu/spring2020/papers/05-mvcc3/p128-bottcher.pdf

[4] - http://justinlevandoski.org/papers/ICDE18_mwcas.pdf

[5] - https://www.microsoft.com/en-us/research/uploads/prod/2018/03/faster-sigmod18.pdf

[6] - https://15721.courses.cs.cmu.edu/spring2019/papers/04-mvcc2/p677-neumann.pdf

[7] - https://db.in.tum.de/~leis/papers/nvm.pdf?lang=en

[8] - https://db.in.tum.de/~leis/papers/leanstore.pdf?lang=en

[9] - https://dl.acm.org/doi/10.1145/3183713.3196896

[10] - https://dl.acm.org/doi/10.1145/3299869.3319870

[11] - https://www.microsoft.com/en-us/research/publication/llama-a-cachestorage-subsystem-for-modern-hardware/

[12] - https://15721.courses.cs.cmu.edu/spring2019/papers/08-oltpindexes2/leis-damon2016.pdf

[13] - https://db.in.tum.de/~leis/papers/ART.pdf

[14] - https://15721.courses.cs.cmu.edu/spring2020/papers/03-mvcc1/wu-vldb2017.pdf

[15] - https://01.org/blogs/2019/introducing-intel-data-streaming-accelerator

[16] - https://db.in.tum.de/~fent/papers/Self%20Tuning%20Art.pdf?lang=en

### Basic end state example

(Look at examples/basic.rs for current state)

```rust
    struct MyData {
        value: usize
    }
    struct MyRoot {
        idx: Index<usize, MyData>
    };

    let librarius = LibrariusBuilder::new()
        .create_with_typed(|| Root::new())
        .source(MemorySource::new(1 << 40)?)
        .source(PMEMSource::new("/mnt/pmem/file")?)
        .source(SDPKSource::new("/dev/something")?)
        .source(IOUringSource::new("/mnt/ssd/file")?)
        .source(RpmaSource::new("...")?)
        .open()?;

    let subscription = librarius.subscribe(|snapshot| {
        let mut root = snapshot.root::<MyRoot>()?;
        let rootref = snapshot.read(root)?;

        rootref.index.prefix(...)
    }); /* async stream */

    let replica = RpmaReplica::new(...);
    some_executor::run(async move {
        while let Some (data) = subscription.await {
                replica.send(data)?.await;
        }
    });

    librarius.run(|tx| {
        let mut root = tx.root::<MyRoot>()?;

        let rootref = tx.read(root)?;

        if let Some(data) = rootref.idx.get(1234)? {
                let dataref = tx.write(data)?;
                dataref.value += 1;
        }

        Ok(())
    })?;
```

### Entity versioning & upgrade example

```rust
    /*
     * This procedural macro will automatically calculate a unique hash of the
     * struct, taking into account its name and fields.
     */
    #[derive(Persistent)]
    #[persistent(version(1))] // this is optional but recommended in case of a hash conflict
    struct MyData_v1 {
        foo: usize,
    }

    #[derive(Persistent)]
    #[persistent(version(2))]
    struct MyData_v2 {
        foo: usize,
        bar: usize,
    }

    let librarius = ...;
    ...

    /*
     * This will be called lazily by a transaction once it encounters an older
     * version of an object.
     */
    librarius.register_upgrade(|tx, data: &MyData_v1| {
        MyData_v2 {
            foo: data.foo,
            bar: 0,
        }
    });

    librarius.run(|tx| {
        ...

        /* the object is guaranteed to be in the new version */
        let data = tx.read(root.data);
        assert_eq!(data.version(), 2);

        data.bar = 5;

        ...
    })?;
```
