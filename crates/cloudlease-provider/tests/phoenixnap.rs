//! The phoenixNAP adapter and reaper against a stateful fake phoenixNAP API and OAuth server

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    future::Future,
    rc::Rc,
    task::{Context, Poll, Waker},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cloudlease_core::{
    alarm::Backoff,
    holding::{Holding, HoldingState, ReleaseReason},
    ids::{Generation, LeaseId, OwnerId, ResourceId, TagPrefix},
    lease::{Lease, LeasePolicy},
    tags::{OwnerTags, TagScheme, UnrecognizedReason},
    time::UnixMillis,
};
use cloudlease_provider::{
    error::ProviderErrorCode,
    http::{
        HttpMethod, HttpRequest, HttpResponse, HttpTransport, LocalFuture, Runtime, TransportError,
    },
    phoenixnap::{
        BootScript, ClientOptions, Hostname, Location, OsImage, PhoenixNap, PhoenixNapClient,
        PnapCredentials, PnapKind, PnapSpec, ServerSpec, ServerType, SshKeyIds,
    },
    provider::{CloudProvider, Presence},
    reaper::reap,
};
use serde_json::{Value, json};

const API: &str = "https://api.phoenixnap.test/";
const TOKEN_URL: &str =
    "https://auth.phoenixnap.test/auth/realms/BMC/protocol/openid-connect/token";
const TOKEN_LIFETIME_SECS: u64 = 900;

struct FakeServer {
    hostname: String,
    status: String,
    tags: Vec<Value>,
    // reads that still see the server after a deprovision, as phoenixNAP lists a server as
    // `deleting` before it is gone
    reads_until_gone: Option<u8>,
}

#[derive(Default)]
struct FakeApi {
    now: Rc<Cell<u64>>,
    servers: BTreeMap<String, FakeServer>,
    // Tag Manager tag names
    tags: BTreeSet<String>,
    // token value to expiry in Unix milliseconds
    tokens: BTreeMap<String, u64>,
    next_id: u64,
    log: Vec<String>,
    create_bodies: Vec<Value>,
    // forced responses by "METHOD path", consumed in order
    failures: BTreeMap<String, Vec<u16>>,
}

impl FakeApi {
    fn handle(&mut self, request: &HttpRequest) -> HttpResponse {
        if request.url.as_str() == TOKEN_URL {
            return self.issue_token(request);
        }

        let path = request.url.path().trim_start_matches('/').to_owned();
        let key = format!("{} {path}", request.method.as_str());
        self.log.push(key.clone());

        let token = request
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .and_then(|(_, value)| value.strip_prefix("Bearer "));
        let live = token
            .and_then(|token| self.tokens.get(token))
            .is_some_and(|expiry| self.now.get() < *expiry);
        if !live {
            // the API gateway's own body shape, not phoenixNAP's error schema
            return respond(
                401,
                json!({ "fault": { "faultstring": "Invalid access token" } }),
            );
        }

        if let Some(status) = self
            .failures
            .get_mut(&key)
            .and_then(|queue| (!queue.is_empty()).then(|| queue.remove(0)))
        {
            return respond(status, json!({ "message": "forced failure" }));
        }

        let segments: Vec<_> = path.split('/').collect();
        match (request.method, segments.as_slice()) {
            (HttpMethod::Get, ["bmc", "v1", "servers"]) => self.list_servers(),
            (HttpMethod::Get, ["bmc", "v1", "servers", id]) => self.read_server(id),
            (HttpMethod::Post, ["bmc", "v1", "servers"]) => self.create_server(request),
            (HttpMethod::Post, ["bmc", "v1", "servers", id, "actions", "deprovision"]) => {
                self.deprovision(id, request)
            }
            (HttpMethod::Get, ["tag-manager", "v1", "tags"]) => self.list_tags(request),
            (HttpMethod::Post, ["tag-manager", "v1", "tags"]) => self.create_tag(request),
            _ => respond(404, json!({ "message": "no route" })),
        }
    }

    fn issue_token(&mut self, request: &HttpRequest) -> HttpResponse {
        self.log.push("POST token".to_owned());
        let expected = format!("Basic {}", STANDARD.encode("client-id:client-secret"));
        let authorized = request
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && *value == expected);
        let body = String::from_utf8(request.body.clone().unwrap_or_default()).unwrap();
        if !authorized || body != "grant_type=client_credentials" {
            return respond(401, json!({ "error": "unauthorized_client" }));
        }

        let token = format!("token-{}", self.tokens.len() + 1);
        self.tokens
            .insert(token.clone(), self.now.get() + TOKEN_LIFETIME_SECS * 1_000);

        respond(
            200,
            json!({
                "access_token": token,
                "expires_in": TOKEN_LIFETIME_SECS,
                "token_type": "Bearer",
            }),
        )
    }

    fn server_json(id: &str, server: &FakeServer) -> Value {
        json!({
            "id": id,
            "hostname": server.hostname,
            "status": server.status,
            "type": "s3.c3.medium",
            "location": "PHX",
            "tags": server.tags,
        })
    }

    fn list_servers(&self) -> HttpResponse {
        let servers: Vec<_> = self
            .servers
            .iter()
            .map(|(id, server)| Self::server_json(id, server))
            .collect();

        respond(200, Value::Array(servers))
    }

    fn read_server(&mut self, id: &str) -> HttpResponse {
        let refused = || {
            respond(
                403,
                json!({ "message": "The request failed since this resource cannot be accessed by the provided credentials." }),
            )
        };
        let Some(server) = self.servers.get_mut(id) else {
            return refused();
        };
        match server.reads_until_gone {
            Some(0) => {
                self.servers.remove(id);
                refused()
            }
            Some(reads) => {
                server.reads_until_gone = Some(reads - 1);
                respond(200, Self::server_json(id, server))
            }
            None => respond(200, Self::server_json(id, server)),
        }
    }

    fn create_server(&mut self, request: &HttpRequest) -> HttpResponse {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        self.create_bodies.push(body.clone());
        let mut tags = Vec::new();
        for tag in body["tags"].as_array().unwrap() {
            let name = tag["name"].as_str().unwrap();
            if !self.tags.contains(name) {
                return respond(
                    400,
                    json!({ "message": "Tag not found", "validationErrors": [name] }),
                );
            }
            tags.push(json!({
                "id": format!("tag-{name}"),
                "name": name,
                "value": tag.get("value"),
                "isBillingTag": false,
                "createdBy": "USER",
            }));
        }

        self.next_id += 1;
        let id = format!("{:024x}", 0x0abc_0000 + self.next_id);
        let server = FakeServer {
            hostname: body["hostname"].as_str().unwrap().to_owned(),
            status: "creating".to_owned(),
            tags,
            reads_until_gone: None,
        };
        let response = Self::server_json(&id, &server);
        self.servers.insert(id, server);

        respond(200, response)
    }

    fn deprovision(&mut self, id: &str, request: &HttpRequest) -> HttpResponse {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body, json!({ "deleteIpBlocks": true }));
        let Some(server) = self.servers.get_mut(id) else {
            return respond(403, json!({ "message": "cannot be accessed" }));
        };
        if server.status == "deleting" {
            return respond(409, json!({ "message": "Server is being deleted" }));
        }

        server.status = "deleting".to_owned();
        server.reads_until_gone = Some(1);

        respond(
            202,
            json!("The server has been deprovisioned and related network setup is being updated."),
        )
    }

    fn list_tags(&self, request: &HttpRequest) -> HttpResponse {
        let name = request
            .url
            .query_pairs()
            .find(|(key, _)| key == "name")
            .map(|(_, value)| value.into_owned());
        let tags: Vec<_> = self
            .tags
            .iter()
            .filter(|tag| name.as_ref().is_none_or(|name| *tag == name))
            .map(|tag| json!({ "id": format!("tag-{tag}"), "name": tag, "isBillingTag": false }))
            .collect();

        respond(200, Value::Array(tags))
    }

    fn create_tag(&mut self, request: &HttpRequest) -> HttpResponse {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["isBillingTag"], json!(false));
        let name = body["name"].as_str().unwrap().to_owned();
        if !self.tags.insert(name.clone()) {
            return respond(409, json!({ "message": "Tag already exists" }));
        }

        respond(201, json!({ "id": format!("tag-{name}"), "name": name }))
    }

    fn add_server(&mut self, id: &str, tags: &[(&str, Option<&str>)]) {
        let tags = tags
            .iter()
            .map(|(name, value)| json!({ "id": "t", "name": name, "value": value, "isBillingTag": false }))
            .collect();
        self.servers.insert(
            id.to_owned(),
            FakeServer {
                hostname: format!("host-{id}"),
                status: "powered-on".to_owned(),
                tags,
                reads_until_gone: None,
            },
        );
    }
}

fn respond(status: u16, body: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: Vec::new(),
        body: serde_json::to_vec(&body).unwrap(),
    }
}

#[derive(Clone, Default)]
struct FakeTransport(Rc<RefCell<FakeApi>>);

impl HttpTransport for FakeTransport {
    fn send(&self, request: HttpRequest) -> LocalFuture<'_, Result<HttpResponse, TransportError>> {
        let response = self.0.borrow_mut().handle(&request);
        Box::pin(async move { Ok(response) })
    }
}

#[derive(Clone, Default)]
struct FakeRuntime {
    now: Rc<Cell<u64>>,
}

impl Runtime for FakeRuntime {
    fn now_millis(&self) -> u64 {
        self.now.get()
    }

    fn sleep(&self, duration: Duration) -> LocalFuture<'_, ()> {
        self.now
            .set(self.now.get() + u64::try_from(duration.as_millis()).unwrap());
        Box::pin(async {})
    }

    fn random_u64(&self) -> u64 {
        0
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}

struct Harness {
    api: Rc<RefCell<FakeApi>>,
    now: Rc<Cell<u64>>,
    provider: PhoenixNap<FakeTransport, FakeRuntime>,
}

impl Harness {
    fn log(&self, prefix: &str) -> Vec<String> {
        self.api
            .borrow()
            .log
            .iter()
            .filter(|entry| entry.starts_with(prefix))
            .cloned()
            .collect()
    }

    fn advance(&self, duration: Duration) {
        self.now
            .set(self.now.get() + u64::try_from(duration.as_millis()).unwrap());
    }
}

fn harness() -> Harness {
    let transport = FakeTransport::default();
    let runtime = FakeRuntime::default();
    runtime.now.set(1_795_521_600_000);
    transport.0.borrow_mut().now = runtime.now.clone();
    let options = ClientOptions::new(PnapCredentials::parse("client-id", "client-secret").unwrap())
        .unwrap()
        .with_api_url(url::Url::parse(API).unwrap())
        .unwrap()
        .with_token_url(url::Url::parse(TOKEN_URL).unwrap())
        .unwrap();
    let client = PhoenixNapClient::new(options, transport.clone(), runtime.clone());

    Harness {
        api: transport.0,
        now: runtime.now,
        provider: PhoenixNap::new(client, TagScheme::new(TagPrefix::parse("cl").unwrap())),
    }
}

fn owner_tags(owner: &str, generation: u64) -> OwnerTags {
    OwnerTags {
        owner: OwnerId::parse(owner).unwrap(),
        generation: Generation::new(generation).unwrap(),
        ceiling: None,
    }
}

fn server_spec(hostname: &str) -> PnapSpec {
    PnapSpec::Server(ServerSpec {
        hostname: Hostname::parse(hostname).unwrap(),
        location: Location::parse("PHX").unwrap(),
        server_type: ServerType::parse("s3.c3.medium").unwrap(),
        os: OsImage::parse("ubuntu/jammy").unwrap(),
        boot: BootScript::parse("#cloud-config\nruncmd:\n  - echo hello\n").unwrap(),
        ssh_key_ids: SshKeyIds::new(vec![ResourceId::parse("5fa54d1e91867c03a0a7b4a4").unwrap()])
            .unwrap(),
    })
}

#[test]
fn creates_an_hourly_server_with_base64_user_data_and_the_exact_tag_pairs() {
    let harness = harness();
    let owner = OwnerTags {
        ceiling: Some(cloudlease_core::tags::Ceiling::at_or_before(
            UnixMillis::new(1_795_525_200_000).unwrap(),
        )),
        ..owner_tags("run_1", 4)
    };

    let id = block_on(harness.provider.create(&owner, &server_spec("clm-run-1-4"))).unwrap();

    let body = harness.api.borrow().create_bodies[0].clone();
    assert_eq!(body["hostname"], "clm-run-1-4");
    assert_eq!(body["type"], "s3.c3.medium");
    assert_eq!(body["location"], "PHX");
    assert_eq!(body["os"], "ubuntu/jammy");
    assert_eq!(body["pricingModel"], "HOURLY");
    assert_eq!(body["installDefaultSshKeys"], false);
    assert_eq!(body["sshKeyIds"], json!(["5fa54d1e91867c03a0a7b4a4"]));
    let user_data = body["osConfiguration"]["cloudInit"]["userData"]
        .as_str()
        .unwrap();
    assert_eq!(
        STANDARD.decode(user_data).unwrap(),
        b"#cloud-config\nruncmd:\n  - echo hello\n"
    );
    assert_eq!(
        body["tags"],
        json!([
            { "name": "cl-managed" },
            { "name": "cl-owner", "value": "run_1" },
            { "name": "cl-gen", "value": "4" },
            { "name": "cl-kind", "value": "server" },
            { "name": "cl-until", "value": "1795525200" },
        ])
    );

    // the five Tag Manager tags were created once, before the first create
    let tags: Vec<_> = harness.api.borrow().tags.iter().cloned().collect();
    assert_eq!(
        tags,
        ["cl-gen", "cl-kind", "cl-managed", "cl-owner", "cl-until"]
    );
    block_on(harness.provider.create(&owner, &server_spec("clm-run-1-5"))).unwrap();
    assert_eq!(harness.log("POST tag-manager").len(), 5);

    let found = block_on(harness.provider.find(
        &OwnerId::parse("run_1").unwrap(),
        Generation::new(4).unwrap(),
        PnapKind::Server,
    ))
    .unwrap();
    assert_eq!(found.len(), 2);
    assert!(found.contains(&id));
}

#[test]
fn find_matches_the_exact_tag_set_and_inventory_reports_near_misses() {
    let harness = harness();
    {
        let mut api = harness.api.borrow_mut();
        let exact = [
            ("cl-managed", None),
            ("cl-owner", Some("run_1")),
            ("cl-gen", Some("2")),
            ("cl-kind", Some("server")),
        ];
        api.add_server("000000000000000000000001", &exact);
        // the same owner as a dashed name rather than a value
        api.add_server(
            "000000000000000000000002",
            &[
                ("cl-managed", None),
                ("cl-owner-run_1", None),
                ("cl-gen", Some("2")),
                ("cl-kind", Some("server")),
            ],
        );
        // a different generation of the same owner
        api.add_server(
            "000000000000000000000003",
            &[
                ("cl-managed", None),
                ("cl-owner", Some("run_1")),
                ("cl-gen", Some("3")),
                ("cl-kind", Some("server")),
            ],
        );
        // an owner that only differs by case
        api.add_server(
            "000000000000000000000004",
            &[
                ("cl-managed", None),
                ("cl-owner", Some("RUN_1")),
                ("cl-gen", Some("2")),
                ("cl-kind", Some("server")),
            ],
        );
        // an extra tag next to an otherwise exact set
        api.add_server(
            "000000000000000000000005",
            &[
                ("cl-managed", None),
                ("cl-owner", Some("run_1")),
                ("cl-gen", Some("2")),
                ("cl-kind", Some("server")),
                ("team", Some("ops")),
            ],
        );
        // someone else's server
        api.add_server("000000000000000000000006", &[("team", Some("ops"))]);
        // a deadline plus a valueless tag shaped like a second one
        api.add_server(
            "000000000000000000000007",
            &[
                ("cl-managed", None),
                ("cl-owner", Some("run_1")),
                ("cl-gen", Some("2")),
                ("cl-kind", Some("server")),
                ("cl-until", Some("1795525200")),
                ("cl-until-123", None),
            ],
        );
    }

    let found = block_on(harness.provider.find(
        &OwnerId::parse("run_1").unwrap(),
        Generation::new(2).unwrap(),
        PnapKind::Server,
    ))
    .unwrap();
    assert_eq!(
        found,
        [ResourceId::parse("000000000000000000000001").unwrap()]
    );

    let inventory = block_on(harness.provider.inventory()).unwrap();
    let managed: Vec<_> = inventory
        .managed
        .iter()
        .map(|observed| observed.id.as_str())
        .collect();
    assert_eq!(
        managed,
        [
            "000000000000000000000001",
            "000000000000000000000003",
            "000000000000000000000004"
        ]
    );
    let unrecognized: Vec<_> = inventory
        .unrecognized
        .iter()
        .map(|entry| (entry.id.as_str(), entry.reason))
        .collect();
    assert_eq!(
        unrecognized,
        [
            ("000000000000000000000002", UnrecognizedReason::ExtraTag),
            ("000000000000000000000005", UnrecognizedReason::ExtraTag),
            ("000000000000000000000007", UnrecognizedReason::ExtraTag),
        ]
    );
}

#[test]
fn a_deprovisioned_server_is_absent_only_once_the_list_drops_it() {
    let harness = harness();
    let id = block_on(
        harness
            .provider
            .create(&owner_tags("run_1", 1), &server_spec("clm-a")),
    )
    .unwrap();

    block_on(harness.provider.delete(PnapKind::Server, &id)).unwrap();
    // still listed as deleting, then gone; the gone read answers 403
    assert_eq!(
        block_on(harness.provider.presence(PnapKind::Server, &id)).unwrap(),
        Presence::Present
    );
    assert_eq!(
        block_on(harness.provider.presence(PnapKind::Server, &id)).unwrap(),
        Presence::Absent
    );
    assert!(harness.api.borrow().servers.is_empty());

    // deleting it again finds it gone rather than failing on the 403
    block_on(harness.provider.delete(PnapKind::Server, &id)).unwrap();
    assert_eq!(
        harness.log("POST bmc/v1/servers/").len(),
        2,
        "one deprovision per delete"
    );
}

#[test]
fn a_second_delete_of_a_deleting_server_succeeds() {
    let harness = harness();
    let id = block_on(
        harness
            .provider
            .create(&owner_tags("run_1", 1), &server_spec("clm-a")),
    )
    .unwrap();

    block_on(harness.provider.delete(PnapKind::Server, &id)).unwrap();
    block_on(harness.provider.delete(PnapKind::Server, &id)).unwrap();
}

#[test]
fn a_refused_read_of_a_listed_server_is_an_error_not_absence() {
    let harness = harness();
    harness
        .api
        .borrow_mut()
        .add_server("000000000000000000000001", &[("cl-managed", None)]);
    harness.api.borrow_mut().failures.insert(
        "GET bmc/v1/servers/000000000000000000000001".to_owned(),
        vec![403],
    );

    let error = block_on(harness.provider.presence(
        PnapKind::Server,
        &ResourceId::parse("000000000000000000000001").unwrap(),
    ))
    .unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::PermissionDenied);
}

#[test]
fn an_expiring_token_is_replaced_before_it_lapses_and_a_rejected_one_once() {
    let harness = harness();
    block_on(harness.provider.inventory()).unwrap();
    block_on(harness.provider.inventory()).unwrap();
    assert_eq!(harness.log("POST token").len(), 1, "the token is reused");

    // inside the refresh margin, a new token is fetched before the request
    harness.advance(Duration::from_secs(TOKEN_LIFETIME_SECS - 30));
    block_on(harness.provider.inventory()).unwrap();
    assert_eq!(harness.log("POST token").len(), 2);
    assert_eq!(
        harness.log("GET bmc/v1/servers").len(),
        3,
        "no request was refused"
    );

    // the API revokes the token early: the create is sent once more with a new token, and
    // exactly one server exists
    harness.api.borrow_mut().tokens.clear();
    block_on(
        harness
            .provider
            .create(&owner_tags("run_1", 1), &server_spec("clm-a")),
    )
    .unwrap();
    assert_eq!(harness.log("POST token").len(), 3);
    assert_eq!(harness.api.borrow().servers.len(), 1);
}

#[test]
fn bad_credentials_fail_without_retrying() {
    let harness = harness();
    let transport = FakeTransport(harness.api.clone());
    let options = ClientOptions::new(PnapCredentials::parse("client-id", "wrong-secret").unwrap())
        .unwrap()
        .with_api_url(url::Url::parse(API).unwrap())
        .unwrap()
        .with_token_url(url::Url::parse(TOKEN_URL).unwrap())
        .unwrap();
    let provider = PhoenixNap::new(
        PhoenixNapClient::new(options, transport, FakeRuntime::default()),
        TagScheme::new(TagPrefix::parse("cl").unwrap()),
    );

    let error = block_on(provider.inventory()).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::PermissionDenied);
    assert_eq!(harness.log("POST token").len(), 1);
    assert!(harness.log("GET").is_empty());
}

#[test]
fn no_stock_is_its_own_error_and_creates_are_never_replayed() {
    let harness = harness();
    harness
        .api
        .borrow_mut()
        .failures
        .insert("POST bmc/v1/servers".to_owned(), vec![406, 503]);

    let error = block_on(
        harness
            .provider
            .create(&owner_tags("run_1", 1), &server_spec("clm-a")),
    )
    .unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::NoCapacity);
    assert!(!error.retryable);

    let error = block_on(
        harness
            .provider
            .create(&owner_tags("run_1", 1), &server_spec("clm-a")),
    )
    .unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::ServerError);
    assert_eq!(harness.log("POST bmc/v1/servers").len(), 2);

    // reads retry through a 503
    harness
        .api
        .borrow_mut()
        .failures
        .insert("GET bmc/v1/servers".to_owned(), vec![503]);
    block_on(harness.provider.inventory()).unwrap();
    assert_eq!(harness.log("GET bmc/v1/servers").len(), 2);
}

#[test]
fn an_expired_holding_finds_a_lost_server_by_its_tags_and_confirms_it_gone() {
    let harness = harness();
    let owner = owner_tags("run_1", 1);
    // the create succeeded but its response was lost, so the holding only requested it
    let server = block_on(harness.provider.create(&owner, &server_spec("clm-a"))).unwrap();
    let start = harness.now.get();
    let at = |offset: u64| UnixMillis::new(start + offset).unwrap();

    let policy = LeasePolicy::new(Duration::from_secs(120), Duration::from_secs(3_600)).unwrap();
    let lease = Lease::start(LeaseId::parse("lease_1").unwrap(), &policy, at(0), None).unwrap();
    let holding = Holding::leased(owner.owner.clone(), owner.generation, lease)
        .request(PnapKind::Server, at(0))
        .unwrap();
    let releasing = holding.expire(at(120_000)).unwrap();
    let backoff = Backoff::default();

    // the server is still listed as deleting right after the deprovision, so the pass
    // schedules a retry
    let first = block_on(reap(&harness.provider, &releasing, at(120_000), &backoff)).unwrap();
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert!(matches!(
        first.holding.state(),
        HoldingState::Releasing { retry: Some(_), .. }
    ));

    let second = block_on(reap(
        &harness.provider,
        &first.holding,
        at(121_000),
        &backoff,
    ))
    .unwrap();
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert_eq!(
        second.holding.state(),
        &HoldingState::Released {
            reason: ReleaseReason::HeartbeatMissed,
            at: at(121_000),
        }
    );
    assert!(harness.api.borrow().servers.is_empty());
    let deprovision = format!("POST bmc/v1/servers/{server}/actions/deprovision");
    assert!(
        harness
            .log("POST bmc/v1/servers/")
            .iter()
            .all(|entry| *entry == deprovision)
    );
}
