//! The Latitude.sh adapter, reaper and sweep executor against a stateful fake Latitude.sh API

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
    tags::{Ceiling, OwnerTags, TagScheme, UnrecognizedReason},
    time::UnixMillis,
};
use cloudlease_provider::{
    error::ProviderErrorCode,
    http::{
        HttpMethod, HttpRequest, HttpResponse, HttpTransport, LocalFuture, Runtime, TransportError,
    },
    latitude::{
        BootScript, ClientOptions, LatKind, LatSpec, Latitude, LatitudeApiKey, LatitudeClient,
        ServerSpec, Slug, UserDataSpec,
    },
    provider::{CloudProvider, Presence},
    reaper::reap,
};
use serde_json::{Value, json};

const API: &str = "https://api.latitude.test/";
// the fake serves at most this many items per page whatever the client asks for
const PAGE_CAP: usize = 2;

#[derive(Default)]
struct FakeApi {
    servers: BTreeMap<String, String>,
    // user data ID to description
    user_data: BTreeMap<String, String>,
    // servers and user data in another project of the same team
    other_project: BTreeSet<String>,
    next_id: u64,
    log: Vec<String>,
    bodies: Vec<Value>,
    // forced error bodies by "METHOD path", consumed in order
    failures: BTreeMap<String, Vec<(u16, Value)>>,
}

impl FakeApi {
    fn handle(&mut self, request: &HttpRequest) -> HttpResponse {
        let path = request.url.path().trim_start_matches('/').to_owned();
        let key = format!("{} {path}", request.method.as_str());
        self.log.push(match request.url.query() {
            Some(query) => format!("{key}?{query}"),
            None => key.clone(),
        });

        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer test-key")
        );
        if let Some((status, body)) = self
            .failures
            .get_mut(&key)
            .and_then(|queue| (!queue.is_empty()).then(|| queue.remove(0)))
        {
            return respond(status, body);
        }

        let segments: Vec<_> = path.split('/').collect();
        match (request.method, segments.as_slice()) {
            (HttpMethod::Get, ["servers"]) => self.list_servers(request),
            (HttpMethod::Get, ["servers", id]) => self.read_server(id),
            (HttpMethod::Post, ["servers"]) => self.create_server(request),
            (HttpMethod::Delete, ["servers", id]) => {
                Self::remove(self.servers.remove(*id).is_some())
            }
            (HttpMethod::Get, ["user_data"]) => self.list_user_data(request),
            (HttpMethod::Get, ["user_data", id]) => self.read_user_data(id),
            (HttpMethod::Post, ["user_data"]) => self.create_user_data(request),
            (HttpMethod::Delete, ["user_data", id]) => {
                Self::remove(self.user_data.remove(*id).is_some())
            }
            _ => not_found(),
        }
    }

    fn page(request: &HttpRequest, items: Vec<Value>) -> HttpResponse {
        let query = |name: &str| {
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == name)
                .and_then(|(_, value)| value.parse::<usize>().ok())
        };
        let size = query("page[size]").unwrap_or(20).min(PAGE_CAP);
        let number = query("page[number]").unwrap_or(1);
        let page: Vec<_> = items
            .into_iter()
            .skip((number - 1) * size)
            .take(size)
            .collect();

        respond(200, json!({ "data": page, "meta": {} }))
    }

    fn server_json(id: &str, hostname: &str) -> Value {
        json!({ "id": id, "type": "servers", "attributes": {
            "hostname": hostname, "status": "on", "tags": [],
        } })
    }

    fn user_data_json(id: &str, description: &str) -> Value {
        json!({ "id": id, "type": "user_data", "attributes": { "description": description } })
    }

    fn filter(request: &HttpRequest, name: &str) -> Option<String> {
        request
            .url
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    // like Latitude.sh, an unfiltered list returns the whole team
    fn in_project(&self, request: &HttpRequest, id: &str) -> bool {
        match Self::filter(request, "filter[project]").as_deref() {
            None => true,
            Some("proj_test") => !self.other_project.contains(id),
            Some(_) => self.other_project.contains(id),
        }
    }

    fn list_servers(&self, request: &HttpRequest) -> HttpResponse {
        let hostname = Self::filter(request, "filter[hostname][eql]");
        let items = self
            .servers
            .iter()
            .filter(|(id, name)| {
                self.in_project(request, id)
                    && hostname.as_ref().is_none_or(|wanted| *name == wanted)
            })
            .map(|(id, name)| Self::server_json(id, name))
            .collect();

        Self::page(request, items)
    }

    fn list_user_data(&self, request: &HttpRequest) -> HttpResponse {
        let items = self
            .user_data
            .iter()
            .filter(|(id, _)| self.in_project(request, id))
            .map(|(id, description)| Self::user_data_json(id, description))
            .collect();

        Self::page(request, items)
    }

    fn read_server(&self, id: &str) -> HttpResponse {
        self.servers.get(id).map_or_else(not_found, |hostname| {
            respond(200, json!({ "data": Self::server_json(id, hostname) }))
        })
    }

    fn read_user_data(&self, id: &str) -> HttpResponse {
        self.user_data
            .get(id)
            .map_or_else(not_found, |description| {
                respond(
                    200,
                    json!({ "data": Self::user_data_json(id, description) }),
                )
            })
    }

    fn create_server(&mut self, request: &HttpRequest) -> HttpResponse {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        self.bodies.push(body.clone());
        let attributes = &body["data"]["attributes"];
        let user_data = attributes["user_data"].as_str().unwrap();
        if !self.user_data.contains_key(user_data) {
            return respond(422, error_body("NOT_FOUND", "User data not found"));
        }

        self.next_id += 1;
        let id = format!("sv_{:04}", self.next_id);
        let hostname = attributes["hostname"].as_str().unwrap().to_owned();
        self.servers.insert(id.clone(), hostname.clone());

        respond(201, json!({ "data": Self::server_json(&id, &hostname) }))
    }

    fn create_user_data(&mut self, request: &HttpRequest) -> HttpResponse {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        self.bodies.push(body.clone());
        self.next_id += 1;
        let id = format!("ud_{:04}", self.next_id);
        let description = body["data"]["attributes"]["description"]
            .as_str()
            .unwrap()
            .to_owned();
        self.user_data.insert(id.clone(), description.clone());

        respond(
            201,
            json!({ "data": Self::user_data_json(&id, &description) }),
        )
    }

    fn remove(existed: bool) -> HttpResponse {
        if existed {
            HttpResponse {
                status: 204,
                headers: Vec::new(),
                body: Vec::new(),
            }
        } else {
            not_found()
        }
    }
}

fn error_body(code: &str, detail: &str) -> Value {
    json!({ "errors": [{ "code": code, "status": "422", "title": "Error", "detail": detail, "meta": {} }] })
}

fn not_found() -> HttpResponse {
    respond(
        404,
        json!({ "errors": [{ "code": "not_found", "status": "404", "title": "Error", "detail": "Specified Record Not Found", "meta": {} }] }),
    )
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
    sleeps: Rc<RefCell<Vec<Duration>>>,
}

impl Runtime for FakeRuntime {
    fn now_millis(&self) -> u64 {
        self.now.get()
    }

    fn sleep(&self, duration: Duration) -> LocalFuture<'_, ()> {
        self.sleeps.borrow_mut().push(duration);
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
    runtime: FakeRuntime,
    provider: Latitude<FakeTransport, FakeRuntime>,
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

    /// Creates a user data record and its server, as a lease does
    fn launch(&self, owner: &OwnerTags) -> (ResourceId, ResourceId) {
        let record = block_on(self.provider.create(owner, &user_data_spec())).unwrap();
        let server = block_on(self.provider.create(owner, &server_spec(&record))).unwrap();

        (record, server)
    }
}

fn harness() -> Harness {
    let transport = FakeTransport::default();
    let runtime = FakeRuntime::default();
    let options = ClientOptions::new(LatitudeApiKey::parse("test-key").unwrap())
        .unwrap()
        .with_base_url(url::Url::parse(API).unwrap())
        .unwrap();
    let client = LatitudeClient::new(options, transport.clone(), runtime.clone());

    Harness {
        api: transport.0,
        runtime,
        provider: Latitude::new(
            client,
            TagScheme::new(TagPrefix::parse("cl").unwrap()),
            Slug::parse("proj_test").unwrap(),
        ),
    }
}

fn owner_tags(owner: &str, generation: u64) -> OwnerTags {
    OwnerTags {
        owner: OwnerId::parse(owner).unwrap(),
        generation: Generation::new(generation).unwrap(),
        ceiling: None,
    }
}

fn user_data_spec() -> LatSpec {
    LatSpec::UserData(UserDataSpec {
        boot: BootScript::parse("#cloud-config\nruncmd:\n  - echo hello\n").unwrap(),
    })
}

fn server_spec(user_data: &ResourceId) -> LatSpec {
    LatSpec::Server(ServerSpec {
        plan: Slug::parse("m4-metal-medium").unwrap(),
        site: Slug::parse("DAL").unwrap(),
        operating_system: Slug::parse("ubuntu_24_04_x64_lts").unwrap(),
        user_data: user_data.clone(),
        ssh_key_ids: vec![ResourceId::parse("ssh_abc").unwrap()],
    })
}

fn at(millis: u64) -> UnixMillis {
    UnixMillis::new(millis).unwrap()
}

#[test]
fn creates_an_hourly_server_named_for_its_generation_from_a_tagged_user_data_record() {
    let harness = harness();
    let owner = OwnerTags {
        ceiling: Some(Ceiling::at_or_before(at(1_795_525_200_000))),
        ..owner_tags("run_1", 4)
    };

    let (record, server) = harness.launch(&owner);

    let bodies = harness.api.borrow().bodies.clone();
    let record_body = &bodies[0]["data"];
    assert_eq!(record_body["type"], "user_data");
    assert_eq!(record_body["attributes"]["project"], "proj_test");
    assert_eq!(
        record_body["attributes"]["description"],
        "cl-managed cl-owner-run_1 cl-gen-4 cl-kind-userdata cl-until-1795525200"
    );
    let content = record_body["attributes"]["content"].as_str().unwrap();
    assert_eq!(
        STANDARD.decode(content).unwrap(),
        b"#cloud-config\nruncmd:\n  - echo hello\n"
    );

    let server_body = &bodies[1]["data"];
    assert_eq!(server_body["type"], "servers");
    let attributes = &server_body["attributes"];
    assert_eq!(attributes["billing"], "hourly");
    assert_eq!(attributes["project"], "proj_test");
    assert_eq!(attributes["plan"], "m4-metal-medium");
    assert_eq!(attributes["site"], "DAL");
    assert_eq!(attributes["operating_system"], "ubuntu_24_04_x64_lts");
    assert_eq!(attributes["user_data"], record.as_str());
    assert_eq!(attributes["ssh_keys"], json!(["ssh_abc"]));
    let hostname = attributes["hostname"].as_str().unwrap();
    assert_eq!(hostname, "cl-c28105877ebc0ba8");
    assert_eq!(
        harness.provider.hostname(&owner.owner, owner.generation),
        hostname
    );

    let inventory = block_on(harness.provider.inventory()).unwrap();
    assert!(inventory.unrecognized.is_empty());
    let managed: Vec<_> = inventory
        .managed
        .iter()
        .map(|observed| (observed.id.clone(), observed.tags.clone()))
        .collect();
    assert_eq!(
        managed,
        [
            (server, owner.with_kind(LatKind::Server)),
            (record, owner.with_kind(LatKind::UserData)),
        ]
    );
}

#[test]
fn find_matches_only_the_exact_owner_and_generation() {
    let harness = harness();
    let (record, server) = harness.launch(&owner_tags("run_1", 2));
    harness.launch(&owner_tags("run_1", 3));
    harness.launch(&owner_tags("RUN_1", 2));
    harness.launch(&owner_tags("run_12", 2));

    let owner = OwnerId::parse("run_1").unwrap();
    let generation = Generation::new(2).unwrap();
    assert_eq!(
        block_on(harness.provider.find(&owner, generation, LatKind::Server)).unwrap(),
        [server]
    );
    assert_eq!(
        block_on(harness.provider.find(&owner, generation, LatKind::UserData)).unwrap(),
        [record]
    );
}

#[test]
fn a_server_whose_create_response_was_lost_is_found_by_its_hostname() {
    let harness = harness();
    let owner = owner_tags("run_1", 1);
    let record = block_on(harness.provider.create(&owner, &user_data_spec())).unwrap();
    // the server exists, but the caller never learned its ID
    let lost = block_on(harness.provider.create(&owner, &server_spec(&record))).unwrap();

    let found = block_on(
        harness
            .provider
            .find(&owner.owner, owner.generation, LatKind::Server),
    )
    .unwrap();
    assert_eq!(found, [lost]);
    let lookups = harness.log("GET servers?");
    assert!(lookups[0].contains("filter%5Bhostname%5D%5Beql%5D=cl-"));
}

#[test]
fn inventory_reads_every_page_and_reports_namespace_servers_without_one_record() {
    let harness = harness();
    for generation in 1..=3 {
        harness.launch(&owner_tags("run_1", generation));
    }
    {
        let mut api = harness.api.borrow_mut();
        // a server in the namespace whose record is gone
        api.servers
            .insert("sv_orphan".to_owned(), "cl-0000000000000000".to_owned());
        // someone else's server and user data
        api.servers
            .insert("sv_other".to_owned(), "web-1".to_owned());
        api.user_data
            .insert("ud_other".to_owned(), "nginx bootstrap".to_owned());
        // a record whose tag set is not exact
        api.user_data.insert(
            "ud_bad".to_owned(),
            "cl-managed cl-owner-run_9 cl-gen-1 cl-kind-userdata extra".to_owned(),
        );
        // another project's deployment of the same app, which its own sweep owns
        api.servers
            .insert("sv_elsewhere".to_owned(), "cl-1111111111111111".to_owned());
        api.user_data.insert(
            "ud_elsewhere".to_owned(),
            "cl-managed cl-owner-run_2 cl-gen-1 cl-kind-userdata".to_owned(),
        );
        api.other_project
            .extend(["sv_elsewhere".to_owned(), "ud_elsewhere".to_owned()]);
    }

    let inventory = block_on(harness.provider.inventory()).unwrap();

    assert_eq!(
        inventory.managed.len(),
        6,
        "three servers and three records"
    );
    let unrecognized: Vec<_> = inventory
        .unrecognized
        .iter()
        .map(|entry| {
            (
                entry.provider_kind.as_str(),
                entry.id.as_str(),
                entry.reason,
            )
        })
        .collect();
    assert_eq!(
        unrecognized,
        [
            ("server", "sv_orphan", UnrecognizedReason::WrongShape),
            ("userdata", "ud_bad", UnrecognizedReason::ExtraTag),
        ]
    );
    // the fake caps pages at two items, so a short page did not end either list
    assert_eq!(harness.log("GET servers?").len(), 4);
    assert_eq!(harness.log("GET user_data?").len(), 4);
}

#[test]
fn two_records_that_disagree_leave_their_server_unrecognized() {
    let harness = harness();
    let owner = owner_tags("run_1", 1);
    let (_, server) = harness.launch(&owner);
    let ceiling = OwnerTags {
        ceiling: Some(Ceiling::at_or_before(at(1_795_525_200_000))),
        ..owner
    };
    block_on(harness.provider.create(&ceiling, &user_data_spec())).unwrap();

    let inventory = block_on(harness.provider.inventory()).unwrap();
    let entry = inventory
        .unrecognized
        .iter()
        .find(|entry| entry.id == server.as_str())
        .unwrap();
    assert_eq!(entry.reason, UnrecognizedReason::DuplicateTag);
}

#[test]
fn a_deleted_server_reads_as_absent_and_a_second_delete_succeeds() {
    let harness = harness();
    let (record, server) = harness.launch(&owner_tags("run_1", 1));

    for (kind, id) in [(LatKind::Server, &server), (LatKind::UserData, &record)] {
        assert_eq!(
            block_on(harness.provider.presence(kind, id)).unwrap(),
            Presence::Present
        );
        block_on(harness.provider.delete(kind, id)).unwrap();
        assert_eq!(
            block_on(harness.provider.presence(kind, id)).unwrap(),
            Presence::Absent
        );
        block_on(harness.provider.delete(kind, id)).unwrap();
    }
}

#[test]
fn no_stock_is_its_own_error_and_rate_limits_wait_as_the_body_asks() {
    let harness = harness();
    let record = block_on(
        harness
            .provider
            .create(&owner_tags("run_1", 1), &user_data_spec()),
    )
    .unwrap();
    harness.api.borrow_mut().failures.insert(
        "POST servers".to_owned(),
        vec![(
            422,
            error_body("SERVERS_OUT_OF_STOCK", "No servers available in stock"),
        )],
    );

    let error = block_on(
        harness
            .provider
            .create(&owner_tags("run_1", 1), &server_spec(&record)),
    )
    .unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::NoCapacity);
    assert!(!error.retryable);
    assert!(harness.api.borrow().servers.is_empty());

    let limited = json!({ "errors": [{
        "code": "RATE_LIMIT_EXCEEDED",
        "status": "429",
        "title": "Rate Limit Exceeded",
        "detail": "Too many requests. Please slow down and retry.",
        "meta": { "retry_after": 7 },
    }] });
    harness
        .api
        .borrow_mut()
        .failures
        .insert("GET user_data/ud_0001".to_owned(), vec![(429, limited)]);
    assert_eq!(
        block_on(harness.provider.presence(LatKind::UserData, &record)).unwrap(),
        Presence::Present
    );
    assert_eq!(
        harness.runtime.sleeps.borrow().as_slice(),
        [Duration::from_secs(7)]
    );
}

#[test]
fn an_expired_holding_deletes_its_server_before_the_record_that_names_it() {
    let harness = harness();
    let owner = owner_tags("run_1", 1);
    let (record, server) = harness.launch(&owner);

    let policy = LeasePolicy::new(Duration::from_secs(120), Duration::from_secs(3_600)).unwrap();
    let lease = Lease::start(LeaseId::parse("lease_1").unwrap(), &policy, at(0), None).unwrap();
    // the server's create response was lost, so the holding only requested it
    let holding = Holding::leased(owner.owner.clone(), owner.generation, lease)
        .request(LatKind::UserData, at(0))
        .unwrap()
        .record_created(LatKind::UserData, record.clone(), at(0))
        .unwrap()
        .request(LatKind::Server, at(0))
        .unwrap();
    let releasing = holding.expire(at(120_000)).unwrap();

    let reaped = block_on(reap(
        &harness.provider,
        &releasing,
        at(120_000),
        &Backoff::default(),
    ))
    .unwrap();

    assert!(reaped.errors.is_empty(), "{:?}", reaped.errors);
    assert_eq!(
        reaped.holding.state(),
        &HoldingState::Released {
            reason: ReleaseReason::HeartbeatMissed,
            at: at(120_000),
        }
    );
    assert_eq!(
        harness.log("DELETE"),
        [
            format!("DELETE servers/{server}"),
            format!("DELETE user_data/{record}")
        ]
    );
    assert!(harness.api.borrow().servers.is_empty());
    assert!(harness.api.borrow().user_data.is_empty());
}
