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
| `cloudlease-provider` | The `CloudProvider` trait, the reaper, the sweep executor and the DigitalOcean, phoenixNAP and Latitude.sh adapters | any, given an HTTP transport |
| `cloudlease-worker` | workers-rs glue: owner alarms, holding storage, the capacity gate, the sweep driver, and building the DigitalOcean and phoenixNAP adapters from bindings | Cloudflare Workers (`worker =0.8.5`) |

The app declares its own Durable Object classes and wraps the handlers in
`cloudlease-worker`, so it keeps its own class names, bindings and settings.
`tests/worker` is a minimal example.

## Apps

| App | What it is |
| --- | --- |
| `apps/machines` (rules in `crates/cloudlease-machines`) | Temporary DigitalOcean and phoenixNAP machines for client apps such as homebased: runs, leases renewed by the machine, a provider per machine class, spend caps and ntfy alerts. See its [README](apps/machines/README.md) |

## Lifetime rules

- A **lease** has a heartbeat deadline and a hard deadline. Clients extend it with
  `heartbeat` (sequenced) or `renew` (unsequenced), and neither can pass the hard
  deadline. The first heartbeat may be given a boot grace, and the hard deadline can be
  capped (`not_after`) by a run deadline.
- A **holding** is one owner generation's resources. It moves from `leased` to `releasing`
  to `released`. It leaves `releasing` only after the provider reports every resource
  absent, deleting in rank order (droplets before volumes). A resource whose create
  response was lost is found by its tags. An empty tag search counts as absence only
  once two minutes have passed since the lease ended, so a create still in flight is
  not missed.
- Leases end when released, when a heartbeat is missed, at the hard deadline, or when
  the owner revokes them.
- The **sweep** lists every resource carrying the managed tag and asks each owner object
  which ones a live lease of the same generation claims. Anything not claimed is deleted.
  An owner object with no record of the resource claims nothing. An owner whose call
  fails or times out is not counted as claiming nothing, so it keeps its resources until
  the next sweep. A resource past its `until` ceiling tag is deleted before any owner is
  asked, so a stalled owner never delays a hard deadline; `SweepConfig::owner_timeout`
  bounds each owner call.
- Each owner also says which of the asked generations it ended itself: still releasing,
  or released after the sweep listed the inventory. A sweep delete of such a generation is
  the owner's own cleanup racing the sweep; every other delete, including a resource still
  listed after its owner confirmed it gone, is reported with `ownerEnded: false`, which apps
  use to alert on orphans.
- **Capacity permits** count against a global limit until the owner proves its resources
  absent. A permit takes one unit by default; a gate that caps spend weighs each permit by
  its price, such as cents per hour. A suspect permit is released only by a sweep whose
  inventory was complete.

## Tags

With prefix `cl`, a managed droplet carries exactly:

```
cl-managed  cl-owner-<owner>  cl-gen-<n>  cl-kind-droplet  [cl-until-<unix secs>]
```

A resource with the managed tag but any other tag shape is reported as unrecognized and
left alone. The exception is a malformed `until` tag on an otherwise exact set: the sweep
never reads it as a deadline or as the absence of one, and asks the owner instead.

## Wiring an app

1. An owner Durable Object per owner ID. It stores holdings in a `HoldingStore`, calls
   `owner::save` after each change, creates resources with `owner::create`, calls
   `owner::run_alarm` from `alarm()`, and answers `POST owner::SWEEP_PATH` with
   `owner::answer_sweep`. `owner::create` saves the create intent before calling the
   provider, so a sweep that runs mid-create, or after the object dies before recording
   the ID, still sees the resource as claimed. The resource's ceiling tag is the lease's
   hard deadline.
2. Optionally, a capacity Durable Object wrapping `capacity::CapacityGate`, used by the app
   through `capacity::CapacityClient`.
3. A cron trigger that calls `sweep::run_sweep` with a `SweepConfig` naming the owner
   namespace and the capacity object.
4. A provider built with `digitalocean::from_env`, or `phoenixnap::from_env`, which returns
   `None` when the Worker has no phoenixNAP credentials.

## Development

Requirements: stable Rust with the `wasm32-unknown-unknown` target, `worker-build` 0.8.5
(`cargo install worker-build --version 0.8.5 --locked`), Node 22 and pnpm.

```sh
just fmt          # check formatting (just fmt-write applies it)
just clippy       # native and wasm32, warnings as errors
just test         # Rust unit and integration tests
just test-worker  # builds tests/worker and apps/machines and runs their miniflare suites
just check        # all of the above
```

The miniflare suite runs the test Worker against a fake DigitalOcean API, installed over
`fetch`, and needs no account or token. It covers:

- an orphan droplet and volume whose owner never existed, swept while another owner is
  mid-create
- resources past their hard deadline or ceiling
- live resources that must be kept
- failing and stalled owners, unrecognized tags and malformed ceilings
- capacity permit release
- the owner alarm's delete-and-confirm loop, and a create that returns after its lease lapsed
- the cron trigger

## Not covered yet

- A Worker on Latitude.sh. Its adapter is tested only against a fake API in
  `crates/cloudlease-provider/tests/`. The machines Worker runs phoenixNAP classes against
  a fake phoenixNAP API in miniflare; no live phoenixNAP server has been created yet
- Firewalls
- Batch-job records and exit status. These belong to the app, which builds on holdings
  and the sweep report
