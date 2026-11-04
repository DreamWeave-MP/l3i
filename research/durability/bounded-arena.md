# Bounded retained-file arena: design for review

Status: **design only**, not an L3i API or a proven durability guarantee. The existing
append-only fixture and `@dream/fs` primitives do not implement this arena. This note does not
choose an application record format or depend on a database driver.

## Goal and preconditions

After a single arena file has been durably created and its name established, checkpoints and
reclamation should not require further namespace changes. Use the same file on Linux, macOS,
and Windows. Hold an exclusive lock for all mutation and recovery. All file writes must be
complete or produce an error; synchronize barriers must succeed before acknowledgment. An
uncertain write or sync poisons the handle. A second writer, concurrent namespace changes, and
active mapped readers over mutable regions are outside the protocol.

The initial durable filename is a separate prerequisite. `fs.createDurable` currently refuses
Windows initialization. Write-through creation plus a file flush is a candidate for investigation,
not an established directory-entry barrier. Do not acknowledge durable initialization on Windows
until a supported mechanism and its failure behavior are established and validated. Existing
files are not automatically known to have a durable name.

## Candidate layout

Reserve a bounded file with independent, aligned regions: two publication records, two checkpoint
slots, and a ring of journal segments. Record the arena identity, format version, generation,
lengths, sequence bounds and checksums. Publication records refer to a checkpoint slot and the
journal range needed to recover it. Layout must account for the reported allocation unit and
power-failure write granularity; merely aligning to 4 KiB is not proof that a write cannot damage
a neighboring region. If the necessary separation cannot be established, refuse the durable
mode rather than overwrite the only recoverable copy.

The consumer supplies opaque data and transaction/checkpoint semantics. A prospective generic
L3i layer would own bounded storage, record integrity and barriers; it must not choose database
schemas, names, or application semantics. This division and the on-disk format require review
before implementation.

## Publication and reclamation

1. Keep the currently published checkpoint and all journal segments needed to recover it intact.
2. Write a complete new checkpoint into the inactive slot. Validate lengths and checksums, then
   synchronize the file. An error or short write makes the outcome uncertain and poisons the
   writer; do not publish or reclaim anything.
3. Write the next publication record into the inactive publication region, referring only to
   synchronized data. Synchronize again. Only then may the caller acknowledge the checkpoint.
4. Retain the previous publication and its data until the new publication is known durable and
   no reader needs the older generation. Only then may its now-unreferenced regions be reused.
5. Reuse journal segments only after the durable checkpoint and publication cover all acknowledged
   records in those segments. If there is not enough free space to construct and publish the new
   generation without overwriting the sole recoverable generation, return a capacity error before
   accepting more data. Do not silently grow the file or retire the old generation early.

Transaction acknowledgment similarly requires a complete, validated record and a successful
file barrier. Group commit may share a barrier but must not acknowledge any member before it
succeeds. An error can leave an unacknowledged complete record; recovery must treat the result as
uncertain, not assert that it failed to reach storage.

## Recovery and failure model

Read publication copies and referenced ranges with retained positional I/O, not memory mappings.
Validate bounds before allocation, arena identity, generation ordering, checksums and application
sequence rules. Select only a complete checkpoint with a valid publication record. A complete
checksum-invalid record is corruption, not an ignorable tail. An incomplete trailing record may be
discarded only under an explicitly defined tail rule. A failed or interrupted publication may
leave old or new state; acknowledgment after a successful publication barrier requires the new
state to be recoverable. If a damaged publication cannot be distinguished safely from an
acknowledged generation, fail closed instead of silently selecting older state.

A simulation must enumerate dropped, torn and reordered writes at both barriers, and verify every
acknowledged transaction survives, incomplete checkpoints never become active, and exhaustion
never causes early reuse. This establishes properties of the model only. It does not establish that
real filesystems, storage devices, or controllers satisfy the model.

## Platform validation still required

- Linux: use a block-layer write log or equivalent crash-replay harness with actual filesystems.
- macOS: confirm the backing filesystem is APFS, exercise file and directory barriers, and use a
  power-cut or equivalent persistence fault setup; process termination alone is insufficient.
- Windows: test NTFS bootstrap and fixed-file barriers as a non-administrative user, including
  creation in an existing durable parent, acknowledgment followed by a power cut, failed writes,
  short writes, full storage, torn publication records and recovery after restart. Also test the
  actual supported Windows and filesystem configurations; unsupported configurations must refuse
  the durable mode. A successful process-kill test or `FlushFileBuffers` alone is not proof that
  a newly created parent directory entry survived power loss.

Until these results and the namespace bootstrap are established, this design is not a basis for
claiming cross-platform durable initialization or bounded-space recovery.
