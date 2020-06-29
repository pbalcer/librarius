# Preface

This document outlines the plan for implementation, and might not be an
accurate representation of the existing code.
I'm also probably forgetting something, so if you see any glaring omission, please
let me know. Same if I'm hopelessly mistaken on something.
Also, this is more of a brain dump rather than a formal design doc, over time
I'll try to organize things better.

Librarius at this stage is just a set of experiments, and things will evolve as
ideas are refined.

## Overview

Librarius Storage Engine is a multi-tier heterogenous persistent heap that supports
asynchronous transactional operations on arbitrary objects and data structures.
Any complex indexing is delegated to a higher level interface.
The design is optimized to work with any configuration of memory and storage devices,
e.g., DRAM-only, DRAM + PMEM, DRAM + SSDs, DRAM + PMEM + SSDs, DRAM + Network and so on.

Librarius is meant to be flexible enough to efficiently support both OLAP and OLTP
type workloads, as well as simpler key-value store type solutions.

## API / User interface

The high level interface exposes memory management interface (alloc/free)
and means of reading and writing to existing memory locations (transactions).
At the low-level, objects are simply array of bytes and the caller can interpret
them in any way, but a convience zero-copy serialization and deserialization
interface is provided.

The current plan is to make the transaction and all of the operations inside of
it asynchronous (e.g., make them return a `Future<T>`), and allow the user to
choose the appropriate executor for their use case to actually run the transaction.
This does not however mean that a single transaction can run multiple parallel
operations. Initially, only `read` will be allowed to run in parallel,
while other operations won't (e.g., they will borrow transaction mutably).
This might change, depending on experiments around executors and scalable memory
allocation.

This is the rough sketch of the API of a transaction:
```rust
impl Transaction {
        async fn read(&self, pointer: &'tx UntypedPointer, size: &ObjectSize) -> Result<&'tx [u8]>;
        async fn read_for_write(&self, pointer: &'tx UntypedPointer, size: &ObjectSize) -> Result<&'tx [u8]>;
        async fn root(&mut self) -> &'tx UntypedPointer;
        async fn write(&mut self, pointer: &'tx mut UntypedPointer, size: &ObjectSize) -> Result<&'tx mut [u8]>;
        async fn alloc(&mut self, size: ObjectSize, pointer: &'tx mut UntypedPointer) -> Result<(UntypedPointer, &'tx mut [u8])>;
        async fn free(&mut self, size: ObjectSizepointer: &'tx mut UntypedPointer) -> Result<()>;
        async fn set(&mut self, owner: &UntypedPointer, offset: usize, src: &'tx [u8]) -> Result<()>;
}
```

Transactions will run either through closures, like so:

```rust
    let (visible, persistent) = librarius.run(|tx| {
        let mut root = tx.root::<MyRoot>().await?;
        let root = tx.write(root).await?;
        root.data = 5;

        Ok(())
    });
    visible.await?;
```

or with RAII interface to allow for interactive sessions.
(TBD: need to figure out how to make this work with async)

From the user's perspective, transactions provide (serializable) isolation
and there's no need to use any locking primitives inside of them. They
behave like an STM or a database transaction. If the storage engine is configured
with any persistent source, transactions will by default provide buffered durable
linearizability, with option to synchronize by `awaiting` on the "persistent" future
returned by the transaction. Deployments without low-latency byte-addressable
persistent source will see reduced performance and increased write amplification if
synchronous durable linearizability is enforced.

For best performance, users should be aware of the performance characteristics
of certains operations. To allow safe parallel read and write access from
multiple threads (which is famously forbidden by Rust's memory model),
acquiring write access to data will create a unique copy for the transaction,
which can be then later commited (or not). This makes librarius an MVCC system.
The `set` operation enables fine-grained modification of data inside of an object
without the need to acquire write access. The set operation will merely log an
action to modify a memory location, and will resolve any conflicts at commit time.
For even greater performance, lattices will be provided, which are simply tuples
of data and merge operator. A transaction is guaranteed not to conflict on
lattice modification. This enables implementation of primitives like atomic increment.

The `read_for_write` function can be used to prevent write skew from occuring.

To support various optimizations, there are some limitations placed on objects:

- all objects can have only one parent (i.e., only one pointer can point to an object)
- all pointers inside of an object must be at its beginning
- builtin zero-copy serialization requires that structs implement the `Persistent` trait,
  which enforces that objects must be copy, and have stable ABI representation.

The reasons for those limitations will be explained later in the document.

To facilitate versioning on structs, the serialization module computes a unique
hash of the details of the structure (fields size, alignment and names) on top
of a user-provided version number. Whenever a structure is modified in code, but
the on-media format is different from what's compiled, the read or write operations
will fail. To prevent that, the application must `impl From<OldVersion> for NewVersion`,
so that a conversion can be performed. This will be done automatically, if such an
implementation exists.
For this mechanism to function, structs must `#[derive(Persistent)]`.

## Logical Address Space and Page Allocation

The heap is composed of I/O sources. The source abstraction is an asynchronous
I/O interface that can be implemented for block-based or byte-addressable devices.
Each source also indicates whether it's persistent or not,
i.e., if it retains its contents across restarts/crashes. This allows the source
abstraction to be used to express an asynchronous interface to a wide variety of
contemporary devices, such as regular DRAM, PMEM, SSDs or even network storage.
The source interface does not assume presence of the file system, which enables
it to be used in conjunction with user-space I/O interfaces like those
provided in SPDK.
Memory-like sources also have no requirements with regards to the underlying
page size, and the storage engine won't do anything that might break apart
pages, like mmap-based defragmentation. This is to allow the use of huge pages
to reduce TLB misses - which is effectively required to support deployments
with tens of terabytes of memory, and hundreds of terabytes of storage.

Sources are organized in a Logical Address Space (LAS, Forest in Polish :-)).
Each source is backed by a slice of the LAS, with persistent sources first. The
exact slice of address space for the source is part of the source's metadata.
Attempt to use multiple sources that map to the same slice will fail, because
it's indicative of mixing multiple separate heaps.
To enable heap growth, LAS can be simply extend by adding new sources at runtime,
or, a source can be implemented that simply extends itself internally on write
(e.g., sparse files or unpopulated virtual memory map). Same for shrinking.

The LAS is *not* mapped into the virtual address space of the application.
Doing so has many problems and limitations (would have to use MAP_FIXED,
use OS page cache etc). This means that logical addresses can't be simply
referenced, and instead the LAS layer implements the functionality of
translating slices of LAS to normal slices (literally - `&[u8]`).
This operation is simply called `read`.

Data in LAS is organized in pages and extents. Extents can span multiple pages.
Slices or allocations cannot span multiple extents, i.e., trying to translate
a logical slice that crosses an extent boundry will result in a runtime failure.
All sources, including byte-addressable memory-like sources, are wrapped in
their own allocators. Those source allocators manage memory/storage in extents using
a buddy-allocation scheme, which provides reasonable performance and space utilization.
Extent allocation in a storage engine can cause fragmentation, the solution to
this is discussed later.

The allocator as of right now incurs no on-media metadata overhead, and instead
relies on upper layers to indicate presence of data on extents. This is a trade-off
between runtime performance/space utilization and startup time. By not storing
any on-media information about allocations, the source allocator needs only
to update the runtime information during allocation or deallocation. However,
on startup with non-empty sources, the content of the extents on those sources
must be inspected to see if there are any allocated objects.
This tradeoff will need to be reevaluated, because for large heaps the startup
time will be unacceptable. But, for now, this makes the design and code simpler.

The LAS only gives out mutable references (`&mut [u8]`) to extents once: on allocation.
Allocations need to be published for a Logical Slice (`LogicalSlice`) to be created.
Such a slice can then be optionally divided into smaller pieces, and used in the
read method. This enforces the invariant that there's only one live mutable
reference for a single memory location, guaranteeing no races.

The LAS allocator interface always allocates the fastest available *persistent*
byte-addressable memory - if no such source is available, regular non-persistent
memory is used. This means that all extents always start their life on memory.
If Persistent Memory (PMEM) is available, this enables low latency write transactions,
because objects are immediately persistent once a transaction is committed.
There's no need to copy data from memory to disk. If PMEM is not available,
extents need to allocate a backing equivalently sized extent on another available
persistent (e.g., block-based) source. The data will be then flushed from the
in-memory extent to the on-storage extent.

The mapping between in-memory extents and on-storage extents is stored in a pagetable.
However, this pagetable contains only the mapping between one logical slice to another.
There's no additional indirection level in the form of Page ID or similar.
Similarly, there's no page header since there's nothing to store in it.

This lack of indirection has consequences for references/pointers between
different extents. An extent that is stored on-disk cannot contain a reference
to something that is in *volatile* memory. Normally this is solved by having
a tuple ID, where the ID itself doesn't change, but the mapping of where it's
located does. This is difficult in this implementation for multiple reasons,
the primary being that the storage engine is generic and not limited to storing
tuples in an index. Also, PMEM allows us to avoid that problem altogether because
data on PMEM never has to change location to become persistent.

Instead, the LAS requires that the upper layer provide the ability to locate
all the pointers inside of an extent and all pointers that point to the extent.
When an extent is moved to storage, its pointers, if needed, are modified
in-place to point to the on-storage locations. This can cause a
cascading flush, when other pointed-to extents also don't have any on-storage locations,
but this should be rare.
This is commonly referred to as pointer swizzling in the database literature.

### Pointers

Pointers are AtomicU64. 0..56 bits are for address, 56..58 bits are tags,
58 bit is used as an object type indicator and 59..64 bits are reserved for future use.

The object type indicator is used to determine whether the pointer contains
an actual object or points to a log of an ongoing transaction (more on that later).

There are three different types of pointers, indicated by their tags.
Volatile Byte Addressable Pointers, Persistent Byte Addressable Pointers, and
Indirect Pointers.

Volatile byte addressable pointers are simple memory addresses, and can be
dereferenced in place without any extra logic.
Persistent byte addressable pointers are self-relative offsets, to dereference them
the value of the pointer is added to the location of the pointer,
like so: `&self as usize + pointer.value`.
Indirect pointers are Logical Slices as described above, and need to be read
through LAS, which will either return a byte-addressable extent if the slice is
byte-addressable or allocate one and fill it with data corresponding to the extent
underlying the provided logical slice.
Such pointer will be then "swizzled" in-place to point to the new byte-addressable
location. The operation of reading, allocating and updating a pointer is encapsulated
into a shared future, which will be returned to any threads that try to do the same
operation in parallel. This way, all threads can proceed without any memory
duplication.
On read, LAS will try to allocate memory in the fastest available byte-addressable
memory.

This mechanism allows reads for byte-addressable pointers to proceed without any extra work.
In addition, Persistent Memory resident byte-addressable pointers do not have to be unswizzled,
because self-relative pointers are always valid even if the actual virtual address
of the source changes between application restarts.

Ultimately, this enables librarius to be used as an in-memory database without
the penalty that comes with indirections commonly used in traditional database
designs.

### Replacement Policy

While byte-addressable memory is available, the entire workload resides in memory,
with on-storage persistent backing if required. Once the workload grows beyond
available memory, replacement policy kicks in.
The LAS keeps a map of x% of extents for each source allocator, ready to be evicted.
This map is called eviction candidate map.
All extents on that map have their pointers unswizzled, which, for in-memory extents,
forces all reads to take the slow path through the LAS read function.
Unswizzling is performed by the upper layer. If such a
read happens, the extent is removed from the candidate map and immediately given back to the
reading thread. The removed candidate is then replaced by another extent.
Each candidate has an associated stamp, given at the time of insertion
by the upper layer.
(this is the read version of current transactions, explained below).

NOTE: initial plan is to chose candidates randomly, and then add some
fragmentation-awareness into it, as described below. The policy could
be characterized as "Random with Second Chance".
Also, it would be tempting to just evict extents that have no swizzled pointers.
This would be possible if all extents in the system formed an acyclic graph.
While individual objects do form such a graph, because they can only ever have
a single parent, extents can have more than one object.

When an allocation request comes in, the first extent from the map is evicted that
satisfies the size requirements of the allocation.
However, before the memory extent is reused, the upper layer is consulted whether
the extent is in use.
The upper layer can then use the stamp it has given the extent to see if all
the readers have finished their work. The stamp is the read
version, and so the transaction layer can check whether there are any active
transactions with version lower than the one indicated in the stamp.
If the extent cannot be reused, it's removed from the map and the process
is restarted.

If the request is bigger than any of the available extents on the list, the buddy
of the largest extent on the list is also evicted. This is repeated until a
large enough memory location is available.
This partially addresses the fragmentation problems, but might cause random
performance fluctuations.
The eviction candidate map should store extents that are of similar access
frequency (with recent accesses given more weight) and which are spatially colocated.
This way, instead of randomly evicting data to reduce memory fragmentation,
the replacement policy will naturally free larger contiguous memory locations.
How to do this is TBD... (generational GC anyone?)

This same replacement policy is used across all source tiers.
To make sure that the replacement policy works across different tiers of
byte-addressable memory (PMEM, DRAM, HBM, etc), memory extents are randomly
inserted into the candidate map.

Then, on read, if a faster memory tier has available memory, the extent is
promoted across tiers, and the old backing extent is freed.

If this caused a byte-addressable extent to now be backed by another
byte-addressable, but persistent, tier, the old, now unused, persistent extent
is moved into its tiers eviction candidate map.

One problem with eviction/promotion across non-byte-addressable tiers is
determining whether a given page is in-use and can be safely changed. Also,
pointers to all extent resident objects must be updated to reflect the new
location. For this reason, moving across non-byte-addressable tiers is
implemented as promotion into byte-addressable tier with a tag indicating
that the extent should be treated as a newly-allocated one and flushed into
an indicated tier.
This is done proactively by a separate working thread.

(TBD: the whole idea of "demotion through promotion" is just a wild guess...
need to prototype it in real code)

The goal of the replacament policy is to pretty much stay out of the way.
Readers should pay a minimal cost when the hot working set comfortably fits in
the available byte-addressable memory.

### "Dirty" pages

LAS assumes that no pages/extents that have an existing storage backing are ever
dirty, and so, during reuse of memory extents, the in-memory data is not flushed
to backing storage. This is to avoid any potentially expensive operations
on the memory allocation path.

This assumption of no dirty pages would disallow any in-place updates of existing
extents, which could be detrimental for performance. However, in the common case,
most changes happen through copy-on-write.

To allow in-place updates, the LAS exposes an in-place update log for extents.
It allows the upper layer to submit modification requests, composed of a logical
slice, and its new contents. This interface returns a `Future` that can be used
by the upper layer to know when the request is processed.

The modification requests are grouped into `Hashmap<Extent, Vec<Modification>>`.
This hashmap is lazily processed by a threadpool, and modifications are safely
applied to on-storage extents.

The assumption at the LAS-level is that all modification requests are backed by
a redo/undo log at the upper layer, and there's no need to do any additional work
to guarantee fault tolerance.

If there's a read request to a backing extent with a pending modification,
the modification is applied to the in-memory extent. This does *not* mean
that the modification can be then discarded...

All sources expose atomic write size and smallest possible write size.
For PMEM, this would be 8 bytes atomic write, 1 byte smallest possible write.
This is used to decide what's the best way of applying the modification.
Sources with atomic write size smaller than the smallest possible write
are disallowed (e.g., disks that theoretically might not support sector write atomicity).
This limitation might be revisited later, but would require additional write
indirection through a double-write buffer.

Extents that do not have any storage-backing must be explicitly flushed.
If the extent is on volatile memory or there's an explicit target source for the
flush, a new extent is allocated from the fastest available storage source,
unless one is provided. This extent is then populated with data.
If the extent is located on PMEM, flushing is done through fine-granularity
byte slice flushing (e.g., using user-space cacheline flushing).

Similarly to in-place updates, an assumption is made that the upper-layer
maintains fault tolerance, and no effort is made to ensure atomic write of
whole extents.

This recovery/update method could be considered similar to commonly
used ARIES recovery protocol.
It was designed to blur the difference between in-place updates on PMEM and
traditional storage.

### Compacting Garbage Collection

LAS provides compacting GC to maintain low fragmentation within extents.
Similarly to the replacement policy, LAS maintains compaction candidate map.
This map is populated whenever the upper layer informs LAS that the contents
of an extent has changed.

TBD: keeping a map of all extents with some free memory will be expensive, but
it's the simplest thing I came up with. Future designs will have to include a
periodically running GC that instead scans the heap... Or just trigger compaction
after an extent reaches a threshold of free space (but that would probably
require slotted layout, or at least a header with free space information...)

If the change has made an extent entirely free, the extent is returned back
to its source allocator.
The map is periodically scanned to see if there are any N extents that can be
merged.
If there are, those extents are placed into a compacting task queue processed
by a threadpool.

The worker thread allocates a new extent that can contain all the data within
the extents that are being merged. It then asks the upper layer to fill
consecutive slices of the newly allocated extent with the still valid data of the
old extents.
Once this is done, the worker thread then asks the upper layer to fix up any
pointers that need changing after the data has been moved. Afterwards,
the old extents are placed into the eviction candidate map to be reused
once there are no more readers on the extents.

### Page-level Compression

TBD: Just a *very* loose idea.

Compression is only worthwhile if an extent can be shrunk by at least a page,
because that's the allocation granularity.
Page compression is implemented at the I/O source level. The source abstraction
enables the write functionality to indicate what was the real consumed amount of
bytes for the write. LAS will then use that information to create a new partial
extent, composed of the pages that were saved due to compression.

## Versioned Object Storage

Versioned Object Storage (VAS) defines objects, pointers (explained earlier),
versions, object allocator and log allocator.

Object is a unit of memory managed by the user. A single extent can contain
one or more objects.

Objects have the following header:

size of the pointers section 0..4 bytes
remaining object size 4..8 bytes
version 8..16 bytes
parent 16..24 bytes
other 24..32 bytes

Just as a remainder, all objects that contain pointers must have those pointers
at the beginning. The size of the object section that contains pointers is then
stored inside of the header. This enables the VAS to unswizzle all pointers inside
of an object. The size of the remaining object content is stored alongside
pointers size.

The *single* parent pointer is used to enable replacement policy implementation
and swizzling in general.

The *other* pointer is used to create a doubly-linked list of versions. During
copy on write, a new version of the object is created, with the *other* pointer
pointing to the old version. The *parent* pointer of the old object is changed
to the new object.
The new version pointer is then swapped with the old version pointer.

This makes VAS a system with newest-to-oldest ordering.

Finally, Versions are 8 bytes, with the most significant bit used as a tag to
indicate whether it's a direct version or an indirect version.
A direct version has the rest of its bits used for a simple version number.
An indirect version uses the rest of its bits as an untyped pointer to another
version on the heap. Typically that another version is a transaction version.
Reading a version number from an indirect pointer requires at most one pointer
dereference.

Real versions start from 1 to max value. Value 0 is reserved to mean that the
object belongs to an uncommited transaction and can be skipped during reading.
Write transactions, when they encounter 0 versions on read-for-write or write
operations, abort. Unless the two write-transactions are not conflicting
through the use of lattices.

An object is valid for the purpose of recovery if it has non-zero version and
a non-null parent that isn't a newer version of itself.
If a parent of an object is an allocation from an uncommited transaction
(i.e., has version 0), then the parent of the parent object is switched to point
to the existing version.

Generally, objects are invalid if their end version is smaller than or equal to
any of the currently running transactions. Invalid versions are removed during
compaction.
Note: Future design will use interval-based algorithm to check if an object
is reachable by any transaction.

Objects with version 0 can be considered to be object/page latches.

### Transactional Object Allocator

VAS creates an object allocator for use by the upper layer. Object allocators
consume an extent (alongside a mutable reference to its data), and carve
out smaller objects out of it.
Objects allocator place objects linearly inside of extents.
Extents themselves have no header, meaning that there's no persistent occupancy
information or similar. This is computed at runtime during compaction.

NOTE: A typical slotted configuration of pages/extents will be considered once
compaction is implemented and if this naive approach results in bad performance.

When the allocator is dropped, the unallocated remainder of the extent is
returned to the VOS to be reused by a subsequent allocator.

### Tiny object optimization

NOTE: this is a half-baked idea on how to reduce per-object overhead...

Having a large header associated with an object means that space amplification
for tiny objects, smaller or similar in size to the header, can be a problem.
Header-less objects, or unversioned objects, are a special variety of objects
that are immutable and must be allocated by a special slab allocation interface.
They do not support `read-for-write` or `write` operations, and they cannot
contain pointers.

This is achieved by a special slab-based extent organization, where the extent
is subdivided into equally sized objects, with a header that consists of
class size information and a bitmap.

All slabs must be registered at runtime by the application.

### Transactional Log Allocator

VAS, alongside object allocator, also exposes a specialized log allocator.
Log allocators create one virtually contigious log that contains all
modifications within a version. A single log is composed of one or more extents,
linked in a list.

0..8 bytes is Version, which is initially zero.
8..16 bytes is checksum, which is initially zero.
16..24 bytes is next, which is initially zero.

All transactional operations are recorded in the log:
Allocations (pointer to the new object, new object),
Deallocations (pointer to the, object to deallocate),
In-place updates (pointer to slice, slice with data),
Reads-for-writes (pointer to the read object)

All newly allocated objects use indirect version that points to the version
of the log in which they were created. Once an indirect version is resolved
to a direct non-zero version, the version is updated in-place.

### The root object

The root object exists in only one persistent source, and is used as an
application's starting point.

Note: if would be nice to have multiple named roots. A numa-aware application
could have a root per-numa node...

The root object is located inside of a `root location`, which is a preallocated
region of 64 bytes.
0..32 bytes are occupied by an object header(with a NULL parent, and
version 1).
32..40 bytes are a pointer to an actual root object.
40..48 bytes are a pointer to the first log in a chain of logs.
48..64 bytes are reserved for future use.

Note: it would be better to have a larger root location inside of each persistent
source, each with one (or more) named root pointers, and multiple "lanes". Lanes could
be then used to allocate logs inside of the transactional log allocator. For small
transactions, all logs could fit into the lane, reducing LAS allocator pressure.
This would also remove the need to update the log chain on persistent commit,
making the whole thing faster (a transaction would then only need two sfences
for commit - one after data is flushed, the second after the lane is flushed).
The lane idea is very likely to be implemented.

### Versions

The VAS contains the latest version in the system, and versions
of all the running transactions (in a lock-free map).
On startup, the latest valid version is taken from the last log in the chain
of logs in the root location.

Versioned Reader contains the read version of the transaction, and expose
a read function that traverses the version chain and returns the appropriate
object for the read version.
Version Reader also performs swizzling.
Once a version reader is dropped, the version of the reader is scheduled
to be removed from the running transactions map.
Note: since this map is going to be only read off-the-main-thread
during gc/compaction/eviction, low-latency write performance is more important
than read performance. Any writes to this map will also trigger an asynchronous
check if any stale versions can be freed.

The read operation returns a Future with the object's real slice. If the object
is in-memory, the future can be resolved immediately, otherwise .awaiting the
future will wait for the data to be fetched from storage.

VAS is also used to create version stamps for replacement policy, and it decides
whether an extent with a given version stamp can be evicted/freed. (i.e., can no
longer be read by any running transaction).

## Transactions

Transactions are built on top of the functionality provided by LAS and VAS, and
are MVCC. The six fundamental operations are:
alloc, free, read, write, read-for-write, and set.

Alloc and free take a reference to the pointer on which to perform the operation.
Pointers are not `Copy` from the user perspective, meaning that once an allocation
is created, it cannot be moved to a different pointer. This ensures that there's
always only one parent for a pointer.

Transactions always acquire a Versioned Reader, which is used for all read
operations, and dropped at the end of the transactions.

The first non-read operation acquires a Log Allocator, and allocates a new
initially zeroed write version. This makes the transaction a write transaction.
The first alloc or write operation acquires an Object Allocator.
Per-transaction allocator provides some degree of allocation scalability, but
also means that transactions cannot use multiple threads for allocation
(which includes write).

Read transactions that encounter an uncommited log can simply walk over the log
to find the real pointer of an object.
Read transactions that encounter a commited log, help apply the log, and proceed
once done. This is conceptually similar to Multi-Word Compare-and-Swap.
Write transactions that encounter a commited log (e.g., another set), check if
the two logs operations are conflict-free, and if so - the write transaction
can proceed, otherwise it will abort.

Sets simply create a new log entry, and append the log pointer to the pointer of
the object it's modifying.

The alloc operation allocates a new object of a given size with an indirect
version and creates a new in-place update log entry for the object's pointer.
Once done, the destination pointer of the allocation is atomically swapped to a
pointer of the log or the real pointer, if the destination pointer is inside of
an object allocated by the same transaction.

Note: Extent with log pointers cannot be evicted.

Write is similar to alloc, but also copies the content of the old object to a
new object, and update the version chain inside of the object.

Read-for-write creates a new log entry, and switches the destination pointer
with the log pointer. This prevents any other conflicting write transactions
from suceeding.
This eliminates the need for a validation step during commit. As a bonus,
there's no need to keep the read-set of a transaction.

Note: is this optimal? probably not, but it's the simplest way to prevent
write skew I came up with that was easy to be made "scalable" (at least for readers).
Once implemented, I need to see if there isn't a livelock issue...
Wild idea: I could probably add a futex-thingy with exponential backoff so that
transactions go to sleep instead of repeatedly aborting on a transaction. But
let's not reinvent fair mutexes :)

Once a read-only transaction finishes, it simply drops its versioned reader,
which will commit its read version, enabling GC/eviction/replacement to run.

A write transaction, once it reaches its end, is guaranteed to commit.
The commit is implemented in VAS, and atomically increments the version counter
and writes the fetched version into the log version. From this point onward,
the transaction is visible, but not yet persistent.

If synchronous linearizable durability is not required, the application can
continue, while flushing takes place.

For persistence, all object and all but first extents of the log are asynchronously
flushed.
While that's happening, the first extent has then its `checksum` field written,
which is calculated from the data from the first extent (the rest of
the transaction data doesn't matter, since all we really care about is the
version..., any half-way flushed data will be discarded because the version
won't be commited).

The first extent of the log is then attached to the chain of the logs in the
root location through its `next` pointer.

Once the first extent metadata is filled, and ordering command is issued
(e.g., sfence), and the extent is flushed. Afterwards, the same is done
to the root location to attach the log.

When the transaction is safely persistent, the content of the log can then
be asynchronously processed by submitting modification requests to the LAS.
The log can only be removed once all modification requests are fullfiled.

## Recovery

The mechanism described is effectively a redo log.
During recovery, the root location is opened and the chain of logs is
processed. It will apply any pending modifications. All logs extents but the
latest one are discarded after processing.

The VAS is updated with the commited version of the last log extent.

## Collaborative transactions

The current design doesn't really allow for any CAS-style lock-free programming.
Why? Because CAS inherently creates dependencies between running write threads.
In a transaction that would mean that if there are any two transactions that
used CAS on a set of dependent objects, those two transactions would themselves
become depedent - i.e., they either both commit or both abort. And this
generalizes beyond just two transactions. What's worse, is that once
a transaction finishes, it has to wait for other codependent transactions
to commit before any progress can be made.

This is implementable, and initially I had an interface:

```rust
        async fn collab(&mut self, pointer: &'tx mut UntypedPointer, size: &ObjectSize) -> Result<&'tx [u8]>;
```

That would enable lock-free programming, but I was worried about runaway dependencies
between transactions, causing livelock issues and high latency.

## Lattices

A lattice is logically an object containing some value T and a merge operator `Fn(T, T) -> T`.
The merge operator needs to be persistent, or at least must be available
during recovery. Otherwise, any conflicts would have to be resolved at commit
time, and the actual merged value would have to be written in the log,
instead of the entire lattice. This would again create codependent transactions.
Right now I'm leaning towards an interface where lattice types would have to
be registered while the librarius is being opened. Then, if there's a
log with a lattice modification, but the merge operator is not registered,
the open would fail.

## Streaming replication

TBD...
But the general idea is to provide an asynchronous stream of commited data,
which can then be consumed (or not) to physically replicate data. Such a replica
can be then opened as a librarius source if needed and it will present some
consistent view of the heap.

## NUMA-awareness

I discussed NUMA a little bit with root objects. In general, whenever "best"
is used in context of memory allocation, this also means "best for the NUMA node
of this thread". The source abstraction requires that sources identify to
which NUMA node they are connected to. This information is then use to determine
what's the best source (Target) for the thread (Initiator), based on data
from libnuma.

However, there's also the problem of scheduling of the Futures produced by the
storage engine. I brought up this topic on Rust Internals forum:
https://internals.rust-lang.org/t/support-for-heterogenous-memory-systems/12460/7

Basically, the problem boils down to communicating to the executor which NUMA
nodes should be preferred for a Future. This is impossible right now, but
should be implementable as a specialized crate that exposes locality-aware
future. Then, the executor could optionally support this type of future.
Right now the plan is to talk with some of the developers behind async ecosystems
in Rust (Tokio, async-std, smol) and convince them to address this gap.
