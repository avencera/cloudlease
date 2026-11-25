# cloudlease machines

A Cloudflare Worker that creates temporary machines on DigitalOcean, phoenixNAP Bare Metal
Cloud, Latitude.sh bare metal or AWS EC2 Spot for a client app and deletes them when their
lease ends. Each machine class names its provider. homebased uses it for batch pools: the client opens a
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
| `POST /v1/leases/{lease_id}/renew` | the lease's token only | `200` with `heartbeat_deadline_at`, `hard_deadline_at` and `idle_hold_until`; `410` once ended |
| `GET /v1/health` | none | `200` or `503` with one boolean per setting; `phoenixnap`, `latitude` and `aws` are `null` when the Worker has no settings for that provider and no class needs them |

Bodies:

- run create: `{request_id, name, class, max_machines, deadline_at, spend_cap_cents?}`.
  `request_id` is a UUID, `deadline_at` RFC 3339 and at most `max_run_hours` away.
  `spend_cap_cents` is a whole number of US cents from 1 up to `run_spend_cap_usd`, which
  applies when it is absent; a retry must repeat it, or leave it out again.
- machine create: `{request_id, hostname, boot: {files: [{path, mode, content}], run: [[argv]]}}`.
  `boot` is at most 32 KiB as JSON, 16 files and 8 commands, and must render to at most
  64 KiB of user data (on Latitude.sh, once wrapped in base64 cloud-config, so about 48 KiB of
  script; on AWS, 16 KiB, EC2's limit); anything else is `400 invalid_boot` before a machine
  is admitted.
- run release: `{"reason": "requested"}`
- lease release: `{"reason": "requested" | "idle" | "done"}`
- renew: `{}`

A lease is `provisioning` until its droplet or server exists or it first renews, then
`running`, `releasing` once it ended, and `released` only after the provider confirms every
resource gone: the droplet and volume, the server, or the instance. Its `release` says why
(`requested`, `idle`, `done`, `heartbeat_lapsed`, `hard_deadline`, `run_released`,
`spend_cap`, `create_failed`, `interrupted`) and who (`client`, `machine`, `cloudlease`).
`interrupted` means the provider took the machine back while its lease was live, such as an
AWS Spot interruption. A run is `open` until its deadline or release, `releasing` while
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
   DigitalOcean and AWS, 20 for phoenixNAP and 15 for Latitude.sh by default. A provisioning lease's
   `heartbeat_deadline_at` is that boot deadline. homebased renews every 30 seconds. A run
   keeps the windows its config and class had when the run was created, so renews never
   read the config. A missed heartbeat ends the lease on the alarm, or on any earlier read
   or renew of it; a release ends it at once.
3. **Tag sweep.** Every 5 minutes the cron trigger lists every droplet, volume and
   phoenixNAP server tagged with `CLOUDLEASE_TAG_PREFIX`, every Latitude.sh server and
   user data record in `LATITUDE_PROJECT`, and every EC2 instance carrying the
   `{prefix}-managed` tag key in each region of `AWS_REGIONS`. It first deletes every one past
   its `until` tag, without asking any run object, then asks each run object which of the
   rest it claims and deletes those no live lease claims. A run object that does not answer
   within 5 seconds keeps its other resources until the next sweep. If one provider's list
   fails, the sweep still deletes what the others listed but returns no hourly-gate permit
   that pass, since a permit's resource may sit at the provider it could not see. Each
   provider that fails to list keeps its own alert streak.
4. **Presence.** On a provider that takes machines back, which is AWS only, the run object
   reads each live lease's machine at every whole minute, from 2 minutes after the lease
   began (EC2 reads are eventually consistent, so a describe just after the launch may miss
   it). One minute grid serves every machine of the run, so one pass reads them all. A
   machine the provider no longer has ends its lease as `interrupted` by `cloudlease`; its
   cleanup confirms it gone and returns its permit. A failed read is retried a minute later,
   and the heartbeat still ends a lease whose machine stopped renewing.
5. **Spend caps.** See below.

A machine is `released` only after the droplet, then the volume, or the server, then its
Latitude.sh user data record, or the EC2 instance, are confirmed absent; until then the run object retries with backoff. phoenixNAP shows a
deprovisioned server as `deleting` for a while, and it counts as present until it leaves
the server list.

## phoenixNAP

A phoenixNAP class names a `server_type`, an ordered list of up to 3 `locations` and an
`os` with cloud-init support (`ubuntu/jammy`). A create tries each location in turn and
moves on only when phoenixNAP answers that the type is out of stock there (HTTP 406),
which means nothing was created; after the last location the lease ends with
`create_failed`, adds nothing to the run's spend and returns its hourly-cap permit at once.
Any other failed create counts its minimum and keeps its permit until cleanup, since it may
have left a server behind. There is no fallback to another provider. Servers have local NVMe and no
volume. Every server gets the five Tag Manager tags `{prefix}-managed`, `-owner`, `-gen`,
`-kind` and `-until`, which the Worker creates on first use, and is deprovisioned with its
IP block.

A Worker without `PNAP_CLIENT_ID` and `PNAP_CLIENT_SECRET` refuses a run or machine on a
phoenixNAP class with `400 unknown_class`, and reports `"phoenixnap": false` on health
when a class needs them.

## Latitude.sh

A Latitude.sh class names a `plan`, an ordered list of up to 3 `sites` and an
`operating_system` that supports user data (`ubuntu_24_04_x64_lts`, which also deploys in
seconds where the plan lists it in `deploys_instantly`). All of the Worker's Latitude.sh
resources live in one project, `LATITUDE_PROJECT`, and every list is filtered to it.

Latitude.sh tags hold no values, so a machine's ownership lives elsewhere:

- The lease first creates a user data record holding the boot script, wrapped in a
  `#cloud-config` document (the only Linux format Latitude.sh documents) that writes it to
  `/var/lib/cloudlease/boot.sh` and runs it once. Its description is
  the machine's tag set (`{prefix}-managed`, `-owner-…`, `-gen-…`, `-kind-userdata`,
  `-until-…`). A record costs nothing.
- It then creates the server with `billing: hourly`, booting from that record. The server's
  hostname is `{prefix}-` and 16 hex digits of a hash of the owner and generation, not the
  client's hostname, so a server whose create answer was lost is still found by name. The
  lease's `hostname` stays the client's machine name; the server's OS hostname is the
  `{prefix}-…` one.
- The sweep reads a server's owner and ceiling from the record its hostname matches.
  Cleanup deletes the server, confirms it gone (`404`), then deletes the record.
- A server with that hostname shape and no record is an orphan once it is 10 minutes old by
  its `created_at`, or at once if Latitude.sh gives none: the sweep deletes it and alerts. This covers a create whose answer was
  lost and that Latitude.sh listed only after the lease's cleanup had given up and deleted
  the record. Any other server in the namespace without exactly one record is reported,
  never deleted. Keep hand-made servers in `LATITUDE_PROJECT` out of the `{prefix}-`
  namespace.

Sites fall through like phoenixNAP locations: only `SERVERS_OUT_OF_STOCK` moves on, and
after the last site the lease ends with `create_failed`, adds nothing to the run's spend
and returns its permit at once; the user data record it made is deleted by its cleanup.

A Worker without `LATITUDE_API_KEY` and `LATITUDE_PROJECT` refuses a Latitude.sh class with
`400 unknown_class`, and reports `"latitude": false` on health when a class needs them.

## AWS

An AWS class names a `region`, up to 4 `instance_types`, up to 3 `zones` each with the
`subnet_id` to launch into, an `image_id` (Canonical's Ubuntu 24.04, pinned; see
`infra/aws-machines`), an optional `root_device_name` (`/dev/sda1`, as on Canonical's images,
when absent), a `root_volume_gib` of at least 8, a `launch_template_id` and its
`launch_template_version` (a number, never `$Latest` or `$Default`, so a template edit
changes nothing until the class names the new version), and a `spot_max_price_usd` from
0.001 (EC2's minimum) to its `price_per_hour_usd`.

Each machine is one Spot instance launched by one instant EC2 Fleet (`CreateFleet`) of one
instance. Every instance type in every zone of the class is one Spot pool, and the fleet
offers them all as overrides of the launch template, with the `price-capacity-optimized`
allocation strategy: EC2 picks the cheapest of the pools least likely to be interrupted, so
the order of `instance_types` and `zones` is no preference. Each override sets the type,
subnet, zone, AMI, root volume, IMDSv2 and the class's max price; the user data travels with
the launch, so one template version serves every machine. The request is one-time and EC2
terminates the instance when it needs the capacity back, so an interrupted instance is simply
gone: EC2 never stops it, keeps its volume or relaunches it. It has IMDSv2, a public IPv4
address from its subnet, the template's security group and an encrypted gp3 root volume and
network interface deleted with it. The instance, its root volume and the fleet carry the
key-value tags `{prefix}-managed=true`, `-owner`, `-gen`, `-kind=instance`, `-until` and
`Name` (the hostname). A fleet cannot tag the Spot request it opens, so cleanup accepts an
untagged one-time request as the instance's own and never cancels it: it closes with the
instance.

Before each launch the run leaves out the pools that took one of its machines back in the last
hour, or are taking one back now: it lists the run's instances by their owner tag, terminated
ones included (EC2 lists them for about an hour), and leaves out the type and zone of each one
whose state reason is `Server.SpotInstanceTermination`. A machine EC2 has given its two-minute
interruption notice still runs, and homebased asks for its replacement within that time, so
one `DescribeSpotInstanceRequests` call over the Spot requests of the run's other live
instances also leaves out the pool of each one whose request is `marked-for-termination`. The
run logs the pools it left out. If that leaves no pool, the launch offers them all. If the
instance list fails, the launch offers every pool; if only the Spot request read fails, such as
for a request EC2 no longer lists, the terminated instances still count.

The fleet reports the pool it launched in by its override, and an instant fleet also reports
the instance's zone and subnet beside it; the launch reads either, so a launched instance is
never taken for a failed create when the override is missing.

A fleet that launches nothing answers with an empty instance set and one error per refused
pool. When any pool refused for Spot capacity or for a Spot price above the max price, the
lease ends with `create_failed`, naming the refused pools; it adds nothing to the run's spend
and returns its permit at once. When every pool failed otherwise, such as a type a zone does
not offer, or the call itself failed, such as a quota or permission error, the create ends
with that error. A lost answer is never replayed: the lease's cleanup finds any instance by
its tags.

The launch template's security group should allow no inbound traffic: a machine reaches its
coordinator outbound. The Worker never changes a security group or a launch template. The
sweep lists instances only: a one-time request closes with its instance, the root volume and
network interface are deleted with it, and EC2 deletes an instant fleet on its own some time
after its instance terminates, so none of them can be left behind.

The Worker uses its own IAM user, never rembuild's. `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY` are optional secrets, and `AWS_REGIONS` lists every region the Worker
launches in and sweeps; it comes from the Worker's settings rather than its classes, so
removing a class never takes its region out of the sweep. `infra/aws-machines` covers one
region, so `AWS_REGIONS` lists only that one until the Terraform covers more. A Worker without the keys, or whose
`AWS_REGIONS` omits a class's region, refuses that class with `400 unknown_class`, and
reports `"aws": false` on health when a class needs them. `infra/aws-machines` holds the
network, security group, launch template and IAM policy as Terraform.

homebased sees an interrupted machine's lease released with reason `interrupted`, which it
reads as an unknown reason that may have lost output, as for `heartbeat_lapsed`. An AWS
machine's `lease.json` also carries `"interruption_notice": "aws_imds"`, so the machine
watches the EC2 instance metadata service for the Spot interruption notice EC2 posts about two
minutes before it takes the instance back.

## Paid-hour idle hold

phoenixNAP and Latitude.sh bill every started hour, so an idle machine there can wait out
the rest of its paid hour for free and take a job that arrives meanwhile. Each renew answer
names until when, as `idle_hold_until`, and homebased keeps an idle machine until then
instead of releasing it at its idle limit. It is the end of the hour counted from the lease's
creation, less a 10 minute release margin. The run's spend counts a machine until it is
confirmed gone, and phoenixNAP took 4m16s from release to `gone_at` in the live test, so the
margin covers that, the machine's 30 second renew round and one retried release.

The provider's billing starts at a moment cloudlease is not told, such as phoenixNAP's
`provisionedOn`, but between the lease's creation, which is saved before the provider create,
and the end of the run's boot grace, since a machine renews only once its server runs. In the
first hour the provider's hour cannot end before the counted one, so the hold applies at once.
After that there is no hold within the boot grace after each counted hour turns, when the
provider's hour may have only just started. Either way the hold never reaches a new provider
hour, nor a new counted hour, so it adds nothing to the run's spend and cannot trip a spend
cap. A hold that would pass the lease's hard deadline, and so the run's deadline, is `null`,
as it always is on DigitalOcean and AWS, which bill by the second. With phoenixNAP's 20
minute boot grace an idle machine is held when its idle limit ends in the first 50 minutes,
or 20 to 50 minutes into a later hour.

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
- Latitude.sh lists an hourly price but does not document how a partial hour rounds, so it
  is counted like phoenixNAP, every started hour in full, until that is measured.
- AWS bills a Linux instance per second with a minimum of 60 seconds, and nothing for a Spot
  instance it interrupts in its first hour. The root volume and public IPv4 address bill
  separately, about $0.021 an hour for 150 GiB of gp3 and the address, so an AWS class's
  price includes them; its `spot_max_price_usd` caps the instance alone. Data sent to the
  internet, about $0.09 per GB after the account's first 100 GB a month, is billed by AWS
  but not counted by either cap.

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
  after the gate answers. A machine counts from its admission, while its create is still
  in flight. A run may spend exactly its cap; when accrued spend passes it, the run object
  ends the run, releases every machine with reason `spend_cap`, and alerts.
- **Per hour**: a rate cap. Every machine not yet confirmed gone holds a permit in the
  `Gate` object weighted by its class's price in cents per hour, rounded up. A create whose
  permit would take the total past `HOURLY_SPEND_CAP_CENTS` is refused with
  `spend_cap_reached`, and alerts. A permit is returned once its machine is confirmed gone,
  or at once when phoenixNAP or Latitude.sh refused the create for stock in every location
  or site, since nothing billed was created. A create that failed any other way keeps its permit until cleanup confirms
  the machine gone, which takes at least the two minute tag search.

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
- a run object failing to answer the sweep (priority 3), a provider the sweep cannot list
  (priority 4), or the sweep failing outright for another reason (priority 4), on the third
  sweep in a row and then hourly (the `Monitor` object counts the streaks)

No alert uses priority 5: the topic may be one a Superalert Mac app follows, which sounds its
alarm for every priority 5 message.

## Configuration

| Name | Kind | What |
| --- | --- | --- |
| `DIGITALOCEAN_TOKEN` | secret | droplet and volume create, read and delete |
| `PNAP_CLIENT_ID`, `PNAP_CLIENT_SECRET` | secrets, optional | phoenixNAP OAuth client credentials (BMC servers and Tag Manager); set both or neither |
| `LATITUDE_API_KEY`, `LATITUDE_PROJECT` | secrets, optional | a Latitude.sh API key and the project ID (`proj_…`) the Worker creates and sweeps in; set both or neither |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | secrets, optional | the access key of the Worker's own IAM user (`infra/aws-machines`); set both or neither |
| `AWS_REGIONS` | var, with the AWS keys | the regions the Worker launches in and sweeps, comma separated, such as `us-east-1` |
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
- `phoenixnap`: `{server_type, locations, os, ssh_key_ids}`, with `locations` such as
  `["SEA", "NLD"]` and `ssh_key_ids` the IDs of account SSH keys, at least one: phoenixNAP
  refuses a Linux server with no SSH key, and the Worker never adds the account's default
  keys
- `latitude`: `{plan, sites, operating_system, ssh_key_ids?}`, with `plan` such as
  `m4-metal-medium`, `sites` such as `["DAL", "CHI"]` and `ssh_key_ids` the IDs of project
  SSH keys
- `aws`: `{region, instance_types, zones: [{zone, subnet_id}], image_id, root_device_name?,
  root_volume_gib, launch_template_id, launch_template_version, spot_max_price_usd}`, such
  as `instance_types: ["c8a.4xlarge", "c7a.4xlarge"]`; see AWS above

`api_base` is the URL machines renew at, written into their `lease.json`. See
`wrangler.jsonc` for an example.

## Boot

The machine's user data is a POSIX `sh` script, which DigitalOcean runs at first boot and
phoenixNAP, Latitude.sh and EC2 pass to cloud-init. It logs to `/var/log/cloudlease-boot.log`, writes
`/etc/cloudlease/lease.json` (`lease_id`, `token`, `api_base`, `hard_deadline_at`,
`heartbeat_ttl_secs`, and `interruption_notice` for a provider that can take the machine back:
`aws_imds` for AWS, absent otherwise) with mode `0600`, formats and mounts the volume when the class has
one, writes the client's files under a `077` umask, and runs its commands in order.

## Tests

`just test-worker` builds this Worker and runs `test/machines.worker.test.ts`,
`test/phoenixnap.worker.test.ts`, `test/latitude.worker.test.ts` and `test/aws.worker.test.ts`
in miniflare against the fake DigitalOcean API in `tests/worker/test`, fake phoenixNAP,
Latitude.sh and EC2 Query APIs and a fake ntfy topic, then
`test/optional-providers.bare.test.ts` against the same Worker without phoenixNAP,
Latitude.sh or AWS credentials. It covers
run and machine create idempotency, renew by lease token and refusal on another lease, a
lapsed heartbeat, an immediate release, the hard deadline, `released` only after the droplet
and volume are gone, `max_machines`, both spend caps and a client's lower run cap, a failed
create, a create whose pending record expired while it waited on the gate, a stalled run
object whose droplet is past its hard deadline, and the sweep's orphan and failure alerts.
The phoenixNAP suite covers a server's create with its location, tags and boot data, a
location out of stock falling through to the next and the last failing the create with no
spend counted, the permit of a create refused for stock coming back at once while one
refused with a 503 stays held until cleanup, renew,
release, heartbeat lapse and hard deadline each deleting the server and confirming it
gone, the sweep deleting unclaimed servers and keeping a claimed one, the sweep still
deleting a DigitalOcean orphan while phoenixNAP fails to list, and hourly billing:
a 5 minute server costs an hour, and the second hour takes a run past its cap, and the
idle hold: ten minutes before the hour ends, at once in the first hour, none inside the release
margin or the boot grace after a later hour turns, and none past the run's deadline (a
droplet's renew names none). The
Latitude.sh suite covers the user data record and then the hourly server booting from it,
a site out of stock falling through and the last one failing the create with its permit
back at once, release and heartbeat lapse each deleting the server, then its record, and
confirming both gone, the sweep deleting an unclaimed record and server past their ceiling,
and a server listed only after its lease's cleanup gave up, swept as an orphan once 10
minutes old. The AWS suite covers a one-time Spot launch with its type, zone, max price,
tags and boot data, capacity errors moving through every zone of a type before the next
type and the last failing the create with its permit back at once, release, heartbeat lapse
and hard deadline each terminating the instance and confirming it gone, an interrupted
instance ending its lease as `interrupted` once past the 2 minute grace and returning its
permit, the pool of an interrupted machine, or of one EC2 has given its interruption notice
and still runs, left out of the run's next launch, a present instance's lease kept with its
next check a minute away, the sweep
terminating an unclaimed instance past its ceiling and keeping a claimed one and another
app's, per-second billing, and boot data over EC2's 16 KiB refused. The bare suite covers
phoenixNAP, Latitude.sh and AWS classes refused without credentials.
