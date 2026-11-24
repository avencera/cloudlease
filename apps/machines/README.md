# cloudlease machines

A Cloudflare Worker that creates temporary machines on DigitalOcean or phoenixNAP Bare
Metal Cloud for a client app and deletes them when their lease ends. Each machine class
names its provider. homebased uses it for batch pools: the client opens a
run, asks for machines in it, and each machine renews its own lease. Cloudflare enforces
every machine's lifetime, so a client or agent that dies leaves nothing running.

The rules live in `crates/cloudlease-machines`; this crate is the Worker around them.

## API

Every route but health takes `Authorization: Bearer <token>`. A client token belongs to one
app and sees only that app's runs; a lease token is written into its machine's
`/etc/cloudlease/lease.json` and works only on that lease.

| Route | Credential | Answer |
| --- | --- | --- |
| `POST /v1/runs` | client | `201` run, `200` for a retry of the same request |
| `GET /v1/runs/{run_id}` | client | `200` run with its machines |
| `POST /v1/runs/{run_id}/release` | client | `202` run; every lease of the run is released |
| `POST /v1/runs/{run_id}/machines` | client | `201` lease, `200` for a retry of the same request |
| `GET /v1/leases/{lease_id}` | client, or the lease's token | `200` lease; `410` to the lease token once the lease ended |
| `POST /v1/leases/{lease_id}/release` | client, or the lease's token | `202` lease, `200` if it had already ended |
| `POST /v1/leases/{lease_id}/renew` | the lease's token only | `200` with the new heartbeat deadline; `410` once ended |
| `GET /v1/health` | none | `200` or `503` with one boolean per setting; `phoenixnap` is `null` when the Worker has no phoenixNAP credentials and no class needs them |

Bodies:

- run create: `{request_id, name, class, max_machines, deadline_at, spend_cap_cents?}`.
  `request_id` is a UUID, `deadline_at` RFC 3339 and at most `max_run_hours` away.
  `spend_cap_cents` is a whole number of US cents from 1 up to `run_spend_cap_usd`, which
  applies when it is absent; a retry must repeat it, or leave it out again.
- machine create: `{request_id, hostname, boot: {files: [{path, mode, content}], run: [[argv]]}}`.
  `boot` is at most 32 KiB as JSON, 16 files and 8 commands, and must render to at most
  64 KiB of user data; anything else is `400 invalid_boot` before a machine is admitted.
- run release: `{"reason": "requested"}`
- lease release: `{"reason": "requested" | "idle" | "done"}`
- renew: `{}`

A lease is `provisioning` until its droplet or server exists or it first renews, then
`running`, `releasing` once it ended, and `released` only after the provider confirms every
resource gone: the droplet and volume, or the server. Its `release` says why (`requested`, `idle`, `done`, `heartbeat_lapsed`,
`hard_deadline`, `run_released`, `spend_cap`, `create_failed`) and who (`client`,
`machine`, `cloudlease`). A run is `open` until its deadline or release, `releasing` while
any machine is not yet released, then `released`.

Errors are `{"error": {"code", "message", "retryable"}}`: `invalid_request`,
`invalid_boot`, `deadline_too_far` and `unknown_class` (400), `unauthorized` (401),
`forbidden` (403), `run_not_found` and `lease_not_found` (404), `run_full`,
`request_conflict` and `run_ended` (409), `lease_ended` (410), `spend_cap_reached` (429),
`internal` (500, retryable) and `busy` (503, retryable). Another app's run or lease answers
`404`; a bearer that is neither a client token nor the lease's own token answers `401`.

IDs route themselves: a run ID is `run_` and 24 hex digits derived from the app and its
`request_id`, and a lease ID is `{run_id}-{generation}`. A run's generations only go up:
one given to a create that was refused or expired is never given out again.

## How a machine's life is enforced

Each run is one Durable Object, the owner of its machines; each machine is one generation of
it, with a core holding.

1. **Hard deadline.** A lease ends at the earliest of the run's `deadline_at` and the class's
   `max_hours` from its create. The run object's alarm fires then and deletes the machine.
   The deadline is also the machine's `until` tag, so the sweep deletes it even if the run
   object is broken.
2. **Heartbeat.** The machine must renew within the heartbeat window, 3 minutes by
   default, and first within its class's boot grace: 8 minutes after create for
   DigitalOcean and 20 for phoenixNAP by default. A provisioning lease's
   `heartbeat_deadline_at` is that boot deadline. homebased renews every 30 seconds. A run
   keeps the windows its config and class had when the run was created, so renews never
   read the config. A missed heartbeat ends the lease on the alarm, or on any earlier read
   or renew of it; a release ends it at once.
3. **Tag sweep.** Every 5 minutes the cron trigger lists every droplet, volume and
   phoenixNAP server tagged with `CLOUDLEASE_TAG_PREFIX`. It first deletes every one past
   its `until` tag, without asking any run object, then asks each run object which of the
   rest it claims and deletes those no live lease claims. A run object that does not answer
   within 5 seconds keeps its other resources until the next sweep. If either provider's
   list fails, the sweep deletes nothing that time.
4. **Spend caps.** See below.

A machine is `released` only after the droplet, then the volume, or the server, are
confirmed absent; until then the run object retries with backoff. phoenixNAP shows a
deprovisioned server as `deleting` for a while, and it counts as present until it leaves
the server list.

## phoenixNAP

A phoenixNAP class names a `server_type`, an ordered list of up to 3 `locations` and an
`os` with cloud-init support (`ubuntu/jammy`). A create tries each location in turn and
moves on only when phoenixNAP answers that the type is out of stock there (HTTP 406),
which means nothing was created; after the last location the lease ends with
`create_failed` and adds nothing to the run's spend. Any other failed create counts its
minimum, since it may have left a server behind. There is no fallback to another provider. Servers have local NVMe and no
volume. Every server gets the five Tag Manager tags `{prefix}-managed`, `-owner`, `-gen`,
`-kind` and `-until`, which the Worker creates on first use, and is deprovisioned with its
IP block.

A Worker without `PNAP_CLIENT_ID` and `PNAP_CLIENT_SECRET` refuses a run or machine on a
phoenixNAP class with `400 unknown_class`, and reports `"phoenixnap": false` on health
when a class needs them.

## Spend caps

Prices come from the class table in `MACHINES_CONFIG`, in US dollars per hour. Every
started billing increment is billed in full:

- DigitalOcean bills droplets per second with a minimum of 60 seconds or $0.01, whichever
  is higher, and a powered-off droplet still bills; volumes bill hourly whether or not
  they are attached (DigitalOcean's billing docs, read 2026-11-24). A class with a volume
  should include the volume's price in its hourly price.
- phoenixNAP bills every started hour in full, with a minimum of one hour, until the
  server is deleted, so a server deleted after 5 minutes costs an hour (phoenixNAP's
  billing docs, read 2026-11-24).

A class may set a `billing_increment_secs` that is a whole multiple of its provider's, such as
7200 for phoenixNAP, never a finer or uneven one, since either could bill less than the provider.

- **Per run**: the run's accrued spend is each machine's price times its billed life, from
  create until it was confirmed gone, in whole increments and at least the minimum. On
  phoenixNAP the spend steps up at each hour from create, and the run object checks it
  then; a run can pass its cap by up to one hour of each machine, since the hour that
  crosses it is billed as it starts. Each create still waiting
  on the gate reserves one minimum, so simultaneous creates cannot pass the cap together. A
  run's cap is the client's `spend_cap_cents`, or else `run_spend_cap_usd`. A create that
  would take the run past it is refused with `spend_cap_reached`, and is checked again
  after the gate answers. When accrued spend reaches the cap, the run object ends the run,
  releases every machine with reason `spend_cap`, and alerts.
- **Per hour**: a rate cap. Every machine not yet confirmed gone holds a permit in the
  `Gate` object weighted by its class's price in cents per hour, rounded up. A create whose
  permit would take the total past `HOURLY_SPEND_CAP_CENTS` is refused with
  `spend_cap_reached`, and alerts. A permit is returned once its machine is confirmed gone.

A run counts every machine at the class price and billing stored when the run was
created. If the class is repriced, rebilled, moved to another provider or removed from the
config while the run is open, the run takes no new machines (`409 run_ended`) and the
client starts a new run.

## Alerts

Alerts go to the ntfy topic in `NTFY_URL`, with `NTFY_TOKEN` as a bearer when set, and to
the Worker log either way:

- a run reaching its spend cap (priority 4)
- a create refused by the hourly cap (priority 4)
- a provider create failure, once per run (priority 3)
- a sweep deleting a resource whose lease no run had already ended, which means it leaked
  (priority 3)
- a run object failing to answer the sweep (priority 3), or the sweep failing outright
  (priority 4), on the third sweep in a row and then hourly (the `Monitor` object counts the
  streaks)

No alert uses priority 5: the topic may be one a Superalert Mac app follows, which sounds its
alarm for every priority 5 message.

## Configuration

| Name | Kind | What |
| --- | --- | --- |
| `DIGITALOCEAN_TOKEN` | secret | droplet and volume create, read and delete |
| `PNAP_CLIENT_ID`, `PNAP_CLIENT_SECRET` | secrets, optional | phoenixNAP OAuth client credentials (BMC servers and Tag Manager); set both or neither |
| `CLIENT_TOKENS` | secret | JSON `{app: sha256 hex of its client token}` |
| `NTFY_URL` | secret | the alert topic URL |
| `NTFY_TOKEN` | secret, optional | an ntfy access token |
| `CLOUDLEASE_TAG_PREFIX` | var | the tag prefix; no other cloudlease app may share it |
| `HOURLY_SPEND_CAP_CENTS` | var | the hourly cap, default 100 |
| `MACHINES_CONFIG` | var, JSON | `api_base`, `max_run_hours`, `max_machines_per_run`, `run_spend_cap_usd`, `heartbeat_ttl_secs` (default 180, at least 60), `boot_grace_secs` (the boot grace of DigitalOcean classes, default 480, at least the heartbeat window), `classes` and the classes each app may use |

Every class has `price_per_hour_usd`, `max_hours`, and optionally `boot_grace_secs` and
`billing_increment_secs`, which default to its provider's. Then by `provider`:

- `digitalocean`, the default when `provider` is absent: `{region, size, image, volume?:
  {size_gib, mount}, ssh_key_ids?, vpc_uuid?}`
- `phoenixnap`: `{server_type, locations, os, ssh_key_ids?}`, with `locations` such as
  `["SEA", "NLD"]` and `ssh_key_ids` the IDs of account SSH keys

`api_base` is the URL machines renew at, written into their `lease.json`. See
`wrangler.jsonc` for an example.

## Boot

The machine's user data is a POSIX `sh` script, which DigitalOcean runs at first boot and
phoenixNAP passes to cloud-init. It logs to `/var/log/cloudlease-boot.log`, writes
`/etc/cloudlease/lease.json` (`lease_id`, `token`, `api_base`, `hard_deadline_at`,
`heartbeat_ttl_secs`) with mode `0600`, formats and mounts the volume when the class has
one, writes the client's files under a `077` umask, and runs its commands in order.

## Tests

`just test-worker` builds this Worker and runs `test/machines.worker.test.ts` and
`test/phoenixnap.worker.test.ts` in miniflare against the fake DigitalOcean API in
`tests/worker/test`, a fake phoenixNAP API and a fake ntfy topic, then
`test/phoenixnap.bare.test.ts` against the same Worker without phoenixNAP credentials. It covers
run and machine create idempotency, renew by lease token and refusal on another lease, a
lapsed heartbeat, an immediate release, the hard deadline, `released` only after the droplet
and volume are gone, `max_machines`, both spend caps and a client's lower run cap, a failed
create, a create whose pending record expired while it waited on the gate, a stalled run
object whose droplet is past its hard deadline, and the sweep's orphan and failure alerts.
The phoenixNAP suite covers a server's create with its location, tags and boot data, a
location out of stock falling through to the next and the last failing the create with no
spend counted, renew,
release, heartbeat lapse and hard deadline each deleting the server and confirming it
gone, the sweep deleting unclaimed servers and keeping a claimed one, and hourly billing:
a 5 minute server costs an hour, and the second hour takes a run past its cap. The bare
suite covers a phoenixNAP class refused without credentials.
