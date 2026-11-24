# cloudlease

Leases and cleanup for short-lived cloud machines that a Cloudflare Worker creates. A
Durable Object holds each lease, deletes what it created once the lease ends, and keeps
retrying until the provider confirms the resources are gone. A scheduled sweep deletes any
tagged resource that no live lease claims. That includes resources whose owner object never
existed, such as leftovers from a crashed create.

## Crates

| Crate | What it holds | Runtime |
| --- | --- | --- |
| `cloudlease-core` | Lease, holding, tag, capacity and sweep rules. No I/O, no clock | any |
| `cloudlease-provider` | The `CloudProvider` trait, the reaper, the sweep executor and the DigitalOcean adapter | any, given an HTTP transport |
| `cloudlease-worker` | workers-rs glue: owner alarms, holding storage, the capacity gate and the sweep driver | Cloudflare Workers (`worker =0.8.5`) |

The app declares its own Durable Object classes and wraps the handlers in
`cloudlease-worker`, so it keeps its own class names, bindings and settings.
`tests/worker` is a complete example.

## Lifetime rules

- A **lease** has a heartbeat deadline and a hard deadline. The first heartbeat may be
  given a boot grace, and the hard deadline can be capped (`not_after`) by a run deadline.
- A **holding** is one owner generation's resources. It moves from `leased` to `releasing`
  to `released`. It leaves `releasing` only after the provider reports every resource
  absent, deleting in rank order (droplets before volumes). A resource whose create
  response was lost is found by its tags.
- Leases end when released, when a heartbeat is missed, at the hard deadline, or when
  the owner revokes them.
- The **sweep** lists every resource carrying the managed tag and asks each owner object
  which ones a live lease of the same generation claims. Anything not claimed is deleted.
  An owner that does not answer keeps its resources until the next sweep. A resource past
  its `until` ceiling tag is deleted without asking.
- **Capacity permits** count against a global limit until the owner proves its resources
  absent. A suspect permit is released only by a sweep whose inventory was complete.

## Tags

With prefix `cl`, a managed droplet carries exactly:

```
cl-managed  cl-owner-<owner>  cl-gen-<n>  cl-kind-droplet  [cl-until-<unix secs>]
```

A resource with the managed tag but any other tag shape is reported as unrecognized and
left alone.

## Wiring an app

1. An owner Durable Object per owner ID. It stores holdings in a `HoldingStore`, calls
   `owner::save` after each change, calls `owner::run_alarm` from `alarm()`, and answers
   `POST owner::SWEEP_PATH` with `owner::answer_sweep`.
2. Optionally, a capacity Durable Object wrapping `capacity::CapacityGate`, used by the app
   through `capacity::CapacityClient`.
3. A cron trigger that calls `sweep::run_sweep` with a `SweepConfig` naming the owner
   namespace and the capacity object.
4. A provider built with `digitalocean::from_env`.

## Development

Requirements: stable Rust with the `wasm32-unknown-unknown` target, `worker-build` 0.8.5
(`cargo install worker-build --version 0.8.5 --locked`), Node 22 and pnpm.

```sh
just fmt          # check formatting (just fmt-write applies it)
just clippy       # native and wasm32, warnings as errors
just test         # Rust unit and integration tests
just test-worker  # builds tests/worker and runs the vitest suite in miniflare
just check        # all of the above
```

The miniflare suite runs the test Worker against a fake DigitalOcean API, installed over
`fetch`, and needs no account or token. It covers:

- an orphan droplet and volume whose owner never existed
- resources past their hard deadline or ceiling
- live resources that must be kept
- failing owners and unrecognized tags
- capacity permit release
- the owner alarm's delete-and-confirm loop
- the cron trigger

## Not covered yet

- Providers other than DigitalOcean, and firewalls
- Batch-job records, exit status and spend caps. These belong to the app, which builds on
  holdings and the sweep report
