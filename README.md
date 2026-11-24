# cloudlease

Leased cloud machines with a watchdog that does not depend on the client. A Cloudflare Worker owns each machine's lease, hard deadline and heartbeat, deletes machines whose lease ends, confirms they are gone, and sweeps any tagged resource it does not own.

Two apps build on the shared core: remote Docker builders (rembuild, moving here from its own repo's control plane) and short batch CPU jobs.
