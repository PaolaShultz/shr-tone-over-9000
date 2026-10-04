# shr-tone-over-9000 in GigPies: activation plan

Planning baseline **2026-10-04 / GP-2026-10-04.1**. All new tasks below are
**planned**, not implemented by this document. [Central inventory](https://github.com/PaolaShultz/gigpies/blob/main/docs/MODULE_IMPLEMENTATION_MAP.md) ·
[Agreed contracts](https://github.com/PaolaShultz/gigpies/blob/main/docs/MODULE_CONTRACTS.md). Existing product roadmaps remain authoritative for
unrelated work; this plan owns only the GigPies integration increments below.

## Objective and boundary

Own mono NAM/cabinet chain preparation and live process. GigPies consumes a named processed source only after explicit routing/latency review. No NAM dependency in PA or Desk and no model downloads in normal tests.

## Source and evidence reviewed

Repository: `/home/shome/p/shr-tone-over-9000`. Inspected HEAD: `3fec963b88777278413412625fe7cf09b8c77bef`.
Clean at inspection; recheck before editing. This dated observation is not a future ownership claim.

Owning documents: README.md, docs/OPERATING_GUIDE.md, assets/model-catalog.tsv; no root AGENTS.md present.

Source inspected: `src/audio.rs::{AudioClient,AudioTelemetry,process}`, `src/nam.rs`, `src/cab.rs`, `src/model_manager.rs`, Cargo.toml.

Implemented mono 48 kHz chain of up to four prepared NAM/cabinet stages, background preparation and telemetry. Uses external native NAM source; standalone JACK workflow is not integrated GigPies source hosting. Repository edition 2021 is retained.

These are source inspection and previously recorded results, not fresh builds or
physical acceptance. The planning session runs documentation checks only.

## Milestones and tasks

Optional later guitar source processor. First useful milestone is intentionally deferred. Activate TONE-01 only for a requested named guitar-chain source, an identified cleared model/IR and exact single-host route/clock contract. Not required for first band mixer or UI.

Task states are execution dependencies: READY has no missing software provider;
WAITING names its precise prerequisite; DEFERRED has an activation condition.
Source delivery and build reservation are additional launch prerequisites on a
peer. Every row has one owner, the repository named in its Owner column. A later
task starts only after the previous artifact is reviewed, never merely delivered.

| Task / priority / state | Owner | Work area, inputs and required artifact | Output and measurable acceptance |
|---|---|---|---|
| TONE-01 / P3 / DEFERRED | shr-tone-over-9000 | Activate TONE-01 only for a requested named guitar-chain source, an identified cleared model/IR and exact single-host route/clock contract. Not required for first band mixer or UI. Existing named source files only after activation; provider contract must be accepted first. | Audit one prepared chain source/lifetime contract and explicit host adapter against C-AUDIO source identity, then small synthetic failure/lifecycle checks. No downloaded model catalog expansion or alternate NAM implementation. |

## Validation and failure behavior

After activation, inspect OPERATING_GUIDE prerequisites without installing/downloading automatically; focused existing unit tests then `CARGO_INCREMENTAL=0 cargo +1.97.1 test --locked --all-targets -j 1` if native prerequisites already exist and scope permits. Native dependency absence is a reported blocker, not permission for install.sh or fetching models.

Preparation/download/authentication stay off audio thread. Chain changes retire off-thread, errors preserve known state or explicit silence. No implicit input/output route or extra JACK owner; missing model never substitutes another tone.

Historical research, auditions, exhaustive matrices, long soaks, full-show renders
and physical/combined-load checks are intentionally outside the normal software
milestones unless their protected behavior changes. Retain their owning documented
on-demand commands; no private media download or test hardware side effect.
Independent builds retain lockfiles and existing repository editions; this plan
does not upgrade dependencies/editions or replace existing intra-repository workspace
paths. The ban is on new sibling-repository path dependencies.

## Resources, review and recovery of work

No initial node/build reservation. On activation use a freed software lane with one jobs=1 build slot and exact reviewed source pin. Provisional first-harness budget ≤1 GiB compiler RSS and ≤512 MiB new output; native DSP builds need fresh inventory. No cache/media mirroring or background bench.

Independent fallback: none; leave deferred until a real consumer and scoped authorization exist. No unbounded render or research assignment.
Before builds check free space and target size; below 20 GiB free or above 5 GiB
output is a review, not permission to delete another task's cache. No reduced
coverage/debug information to make a budget appear to pass.

Handoff: exact changed files, commit plus patch hashes or bounded source manifest
if uncommitted, contract IDs/versions and provider-fixture hashes, commands/results,
intentional skipped classes, remaining limits and next task/owner. Stage only named
owned changes if a later implementation session commits; no public push is implied.
Receiving owner reviews independently and writes an immutable private-ledger
acknowledgement. Interrupted work stays visible with last completed acceptance
criterion; never reset/stash/clean another session or replay an uncertain mutation.

## Activation boundary

Activate TONE-01 only for a requested named guitar-chain source, an identified cleared model/IR and exact single-host route/clock contract. Not required for first band mixer or UI. No runtime change or compatibility claim is made by writing this plan.

## Implementation launch prompt

Host/cwd assignments and source preparation are in GigPies PARALLEL_WORK_PLAN.md.
This is a prompt for a later user-started session; no implementation worker has
been started by the planning pass.

```text
Read docs/plans/GIGPIES_IMPLEMENTATION.md in shr-tone-over-9000, check current source and owning instructions,
and verify whether its activation condition is satisfied. There is no READY
implementation task for this repository in the initial GigPies wave. Do not
invent one. If still deferred, report that fact and stop. If activated by an
explicit later GigPies task, implement only the named bounded task after its
contract/ownership review; preserve other sessions and unrelated work. No sibling
writes, unilateral contract changes or sibling path dependencies. Use Rust 1.97.1,
Cargo.lock and CARGO_INCREMENTAL=0 with the shared build slot and cargo -j 1;
respect any stricter owning build restriction. No physical audio/MIDI/DMX,
playback, devices, services, shared load, publication or deployment. Never mark
mocked/incomplete work done; report exact dependency mismatches and keep progress
in this plan. There is no busywork fallback; remain deferred.
```

## Progress

- 2026-10-04: source and owner documents inspected; plan written. Implementation
  tasks remain in the states above. Physical evidence retains its original limits.
