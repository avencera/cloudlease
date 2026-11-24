# cloudlease machines

A Cloudflare Worker that creates temporary DigitalOcean machines for a client app and
deletes them when their lease ends. homebased uses it for batch pools: the client opens a
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
| `GET /v1/health` | none | `200` or `503` with one boolean per setting |

Bodies:

- run create: `{request_id, name, class, max_machines, deadline_at}`. `request_id` is a
  UUID, `deadline_at` RFC 3339 and at most `max_run_hours` away.
- machine create: `{request_id, hostname, boot: {files: [{path, mode, content}], run: [[argv]]}}`
- run release: `{"reason": "requested"}`
- lease release: `{"reason": "requested" | "idle" | "done"}`
- renew: `{}`

A lease is `provisioning` until its droplet exists or it first renews, then `running`,
`releasing` once it ended, and `released` only after DigitalOcean confirms the droplet and
volume gone. Its `release` says why (`requested`, `idle`, `done`, `heartbeat_lapsed`,
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
`request_id`, and a lease ID is `{run_id}-{generation}`.

## How a machine's life is enforced

Each run is one Durable Object, the owner of its machines; each machine is one generation of
it, with a core holding.

1. **Hard deadline.** A lease ends at the earliest of the run's `deadline_at` and the class's
   `max_hours` from its create. The run object's alarm fires then and deletes the machine.
   The deadline is also the droplet's `until` tag, so the sweep deletes it even if the run
   object is broken.
2. **Heartbeat.** The machine must renew within 10 minutes (15 minutes after create, for
   boot). A missed heartbeat ends the lease on the alarm; a release ends it at once.
3. **Tag sweep.** Every 5 minutes the cron trigger lists every droplet and volume tagged
   with `CLOUDLEASE_TAG_PREFIX` and deletes any that no live lease claims.
4. **Spend caps.** See below.

A machine is `released` only after the droplet, then the volume, are confirmed absent;
until then the run object retries with backoff.

## Spend caps

Prices come from the class table in `MACHINES_CONFIG`, in US dollars per hour. DigitalOcean
bills droplets per second with a 60-second minimum, and a powered-off droplet still bills;
volumes bill hourly whether or not they are attached (DigitalOcean's billing docs, read
2026-11-24). A class with a volume should include the volume's price in its hourly price.

- **Per run**: the run's accrued spend is each machine's price times its billed life, at
  least 60 seconds, from create until it was confirmed gone. A create that would take the
  run past `run_spend_cap_usd` is refused with `spend_cap_reached`. When accrued spend
  reaches the cap, the run object ends the run, releases every machine with reason
  `spend_cap`, and alerts.
- **Per hour**: a rate cap. Every machine not yet confirmed gone holds a permit in the
  `Gate` object weighted by its class's price in cents per hour, rounded up. A create whose
  permit would take the total past `HOURLY_SPEND_CAP_CENTS` is refused with
  `spend_cap_reached`, and alerts. A permit is returned once its machine is confirmed gone.

## Alerts

Alerts go to the ntfy topic in `NTFY_URL`, with `NTFY_TOKEN` as a bearer when set, and to
the Worker log either way:

- a run reaching its spend cap (priority 5)
- a create refused by the hourly cap (priority 5)
- a provider create failure, once per run (priority 4)
- a sweep deleting a resource that no run was already releasing, which means it leaked
  (priority 4)
- a run object failing to answer the sweep, or the sweep failing outright, on the third
  sweep in a row and then hourly (the `Monitor` object counts the streaks)

## Configuration

| Name | Kind | What |
| --- | --- | --- |
| `DIGITALOCEAN_TOKEN` | secret | droplet and volume create, read and delete |
| `CLIENT_TOKENS` | secret | JSON `{app: sha256 hex of its client token}` |
| `NTFY_URL` | secret | the alert topic URL |
| `NTFY_TOKEN` | secret, optional | an ntfy access token |
| `CLOUDLEASE_TAG_PREFIX` | var | the tag prefix; no other cloudlease app may share it |
| `HOURLY_SPEND_CAP_CENTS` | var | the hourly cap, default 100 |
| `MACHINES_CONFIG` | var, JSON | `api_base`, `max_run_hours`, `max_machines_per_run`, `run_spend_cap_usd`, `classes` and the classes each app may use |

A class is `{region, size, image, price_per_hour_usd, max_hours, volume?: {size_gib,
mount}, ssh_key_ids?, vpc_uuid?}`. `api_base` is the URL machines renew at, written into
their `lease.json`. See `wrangler.jsonc` for an example.

## Boot

The droplet's user data is a POSIX `sh` script that logs to `/var/log/cloudlease-boot.log`.
It writes `/etc/cloudlease/lease.json` (`lease_id`, `token`, `api_base`,
`hard_deadline_at`, `heartbeat_ttl_secs`) with mode `0600`, formats and mounts the volume
when the class has one, writes the client's files under a `077` umask, and runs its
commands in order.

## Tests

`just test-worker` builds this Worker and runs `test/machines.worker.test.ts` in miniflare
against the fake DigitalOcean API in `tests/worker/test` and a fake ntfy topic. It covers
run and machine create idempotency, renew by lease token and refusal on another lease, a
lapsed heartbeat, an immediate release, the hard deadline, `released` only after the droplet
and volume are gone, `max_machines`, both spend caps, a failed create, and the sweep's
orphan and failure alerts.
