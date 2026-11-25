//! The DigitalOcean adapter and reaper against a stateful fake API

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::Future,
    rc::Rc,
    task::{Context, Poll, Waker},
    time::Duration,
};

use cloudlease_core::{
    alarm::Backoff,
    deletes::DeleteTarget,
    holding::{Holding, HoldingState, ReleaseReason},
    ids::{Generation, LeaseId, OwnerId, TagPrefix},
    lease::{Lease, LeasePolicy},
    tags::{OwnerTags, TagScheme, UnrecognizedReason},
    time::UnixMillis,
};
use cloudlease_provider::{
    digitalocean::{
        BootScript, ClientOptions, DigitalOcean, DigitalOceanClient, DigitalOceanToken, DoKind,
        DoSpec, DropletImage, DropletSpec, ResourceName, Slug,
    },
    error::ProviderErrorCode,
    http::{
        HttpMethod, HttpRequest, HttpResponse, HttpTransport, LocalFuture, Runtime, TransportError,
    },
    provider::CloudProvider,
    reaper::reap,
    rest::RetryPolicy,
};
use serde_json::{Value, json};

const API: &str = "https://api.digitalocean.test/v2/";

struct FakeDroplet {
    name: String,
    tags: Vec<String>,
    // reads that still see the droplet after a delete request, like DigitalOcean's
    // asynchronous deletion
    reads_until_gone: Option<u8>,
}

#[derive(Default)]
struct FakeApi {
    droplets: BTreeMap<u64, FakeDroplet>,
    next_id: u64,
    log: Vec<String>,
    // forced responses by "METHOD path", consumed in order
    failures: BTreeMap<String, Vec<u16>>,
    page_size: usize,
}

impl FakeApi {
    fn handle(&mut self, request: &HttpRequest) -> HttpResponse {
        let path = request.url.path().trim_start_matches("/v2/").to_owned();
        let key = format!("{} {path}", request.method.as_str());
        self.log.push(key.clone());

        if let Some(status) = self
            .failures
            .get_mut(&key)
            .and_then(|queue| (!queue.is_empty()).then(|| queue.remove(0)))
        {
            return respond(
                status,
                json!({ "id": "forced", "message": "forced failure" }),
            );
        }

        let segments: Vec<_> = path.split('/').collect();
        match (request.method, segments.as_slice()) {
            (HttpMethod::Get, ["droplets"]) => self.list_droplets(request),
            (HttpMethod::Get, ["droplets", id]) => self.read_droplet(id),
            (HttpMethod::Delete, ["droplets", id]) => self.delete_droplet(id),
            (HttpMethod::Post, ["droplets"]) => self.create_droplet(request),
            _ => respond(404, json!({ "id": "not_found" })),
        }
    }

    fn page(&self, request: &HttpRequest, total: usize) -> (usize, usize, Option<String>) {
        let page_size = if self.page_size == 0 {
            200
        } else {
            self.page_size
        };
        let page: usize = request
            .url
            .query_pairs()
            .find(|(name, _)| name == "page")
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(1);
        let start = (page - 1) * page_size;
        let end = (start + page_size).min(total);
        let next = (end < total).then(|| {
            let mut url = request.url.clone();
            let pairs: Vec<_> = url
                .query_pairs()
                .filter(|(name, _)| name != "page")
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect();
            url.query_pairs_mut()
                .clear()
                .extend_pairs(pairs)
                .append_pair("page", &(page + 1).to_string());
            url.to_string()
        });

        (start, end, next)
    }

    fn list_droplets(&self, request: &HttpRequest) -> HttpResponse {
        let tag = request
            .url
            .query_pairs()
            .find(|(name, _)| name == "tag_name")
            .map(|(_, value)| value.into_owned());
        let matching: Vec<_> = self
            .droplets
            .iter()
            .filter(|(_, droplet)| tag.as_ref().is_none_or(|tag| droplet.tags.contains(tag)))
            .map(|(id, droplet)| {
                json!({ "id": id, "name": droplet.name, "status": "active", "tags": droplet.tags })
            })
            .collect();
        let (start, end, next) = self.page(request, matching.len());

        respond(
            200,
            json!({ "droplets": matching[start..end], "links": { "pages": { "next": next } } }),
        )
    }

    fn read_droplet(&mut self, id: &str) -> HttpResponse {
        let Some(numeric) = id.parse::<u64>().ok() else {
            return respond(404, json!({ "id": "not_found" }));
        };
        let Some(droplet) = self.droplets.get_mut(&numeric) else {
            return respond(404, json!({ "id": "not_found" }));
        };
        match droplet.reads_until_gone {
            Some(0) => {
                self.droplets.remove(&numeric);
                respond(404, json!({ "id": "not_found" }))
            }
            Some(reads) => {
                droplet.reads_until_gone = Some(reads - 1);
                let body = json!({ "droplet": {
                    "id": numeric, "name": droplet.name, "status": "active", "tags": droplet.tags,
                } });
                respond(200, body)
            }
            None => respond(
                200,
                json!({ "droplet": {
                    "id": numeric, "name": droplet.name, "status": "active", "tags": droplet.tags,
                } }),
            ),
        }
    }

    fn delete_droplet(&mut self, id: &str) -> HttpResponse {
        let droplet = id
            .parse::<u64>()
            .ok()
            .and_then(|numeric| self.droplets.get_mut(&numeric));
        let Some(droplet) = droplet else {
            return respond(404, json!({ "id": "not_found" }));
        };
        droplet.reads_until_gone.get_or_insert(1);

        respond(204, Value::Null)
    }

    fn create_droplet(&mut self, request: &HttpRequest) -> HttpResponse {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        self.next_id += 1;
        let id = 500 + self.next_id;
        let tags: Vec<String> = serde_json::from_value(body["tags"].clone()).unwrap();
        self.droplets.insert(
            id,
            FakeDroplet {
                name: body["name"].as_str().unwrap().to_owned(),
                tags: tags.clone(),
                reads_until_gone: None,
            },
        );

        respond(
            202,
            json!({ "droplet": { "id": id, "name": body["name"], "status": "new", "tags": tags } }),
        )
    }
}

fn respond(status: u16, body: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: Vec::new(),
        body: if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body).unwrap()
        },
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
    provider: DigitalOcean<FakeTransport, FakeRuntime>,
}

fn harness() -> Harness {
    let transport = FakeTransport::default();
    let runtime = FakeRuntime::default();
    let options = ClientOptions::new(DigitalOceanToken::parse("test-token").unwrap())
        .unwrap()
        .with_base_url(url::Url::parse(API).unwrap())
        .unwrap();
    let client = DigitalOceanClient::new(options, transport.clone(), runtime.clone());

    Harness {
        api: transport.0,
        runtime,
        provider: DigitalOcean::new(client, scheme()),
    }
}

fn scheme() -> TagScheme {
    TagScheme::new(TagPrefix::parse("cl").unwrap())
}

fn at(millis: u64) -> UnixMillis {
    UnixMillis::new(millis).unwrap()
}

fn owner_tags(owner: &str, generation: u64) -> OwnerTags {
    OwnerTags {
        owner: OwnerId::parse(owner).unwrap(),
        generation: Generation::new(generation).unwrap(),
        ceiling: None,
    }
}

fn droplet_spec(name: &str) -> DoSpec {
    DoSpec::Droplet(DropletSpec {
        name: ResourceName::parse(name).unwrap(),
        region: Slug::parse("nyc3").unwrap(),
        size: Slug::parse("c-16").unwrap(),
        image: DropletImage::Slug(Slug::parse("ubuntu-24-04-x64").unwrap()),
        boot: BootScript::parse("#!/bin/sh\necho hello\n").unwrap(),
        ssh_key_ids: Vec::new(),
        vpc_uuid: None,
    })
}

fn deletes(harness: &Harness) -> Vec<String> {
    harness
        .api
        .borrow()
        .log
        .iter()
        .filter(|entry| entry.starts_with("DELETE"))
        .cloned()
        .collect()
}

#[test]
fn creates_resources_with_the_exact_owner_tags() {
    let harness = harness();
    let mut owner = owner_tags("job_1", 3);
    owner.ceiling = Some(cloudlease_core::tags::Ceiling::at_or_before(at(
        1_764_000_000_000,
    )));

    let droplet = block_on(harness.provider.create(&owner, &droplet_spec("job-1-a"))).unwrap();
    let inventory = block_on(harness.provider.inventory()).unwrap();

    assert_eq!(
        harness.api.borrow().droplets[&droplet.as_str().parse::<u64>().unwrap()].tags,
        vec![
            "cl-managed",
            "cl-owner-job_1",
            "cl-gen-3",
            "cl-kind-droplet",
            "cl-until-1764000000",
        ]
    );
    assert_eq!(inventory.managed.len(), 1);
    assert_eq!(inventory.managed[0].id, droplet);
    assert_eq!(inventory.managed[0].tags, owner.with_kind(DoKind::Droplet));
    assert!(inventory.is_complete());
}

#[test]
fn inventory_reports_wrong_shapes_and_ignores_foreign_resources() {
    let harness = harness();
    {
        let mut api = harness.api.borrow_mut();
        api.page_size = 1;
        api.droplets.insert(
            1,
            FakeDroplet {
                name: "hand-made".to_owned(),
                tags: vec!["cl-managed".to_owned(), "team-x".to_owned()],
                reads_until_gone: None,
            },
        );
        // a droplet tagged as a kind the adapter does not manage is not safe to manage
        let mislabeled = scheme()
            .render(&owner_tags("job_1", 1), DoKind::Droplet)
            .into_iter()
            .map(|tag| tag.replace("cl-kind-droplet", "cl-kind-volume"))
            .collect();
        api.droplets.insert(
            2,
            FakeDroplet {
                name: "mislabeled".to_owned(),
                tags: mislabeled,
                reads_until_gone: None,
            },
        );
    }

    let inventory = block_on(harness.provider.inventory()).unwrap();

    assert!(inventory.managed.is_empty());
    assert_eq!(
        inventory
            .unrecognized
            .iter()
            .map(|entry| (entry.id.as_str(), entry.reason))
            .collect::<Vec<_>>(),
        vec![
            ("1", UnrecognizedReason::ExtraTag),
            ("2", UnrecognizedReason::InvalidValue),
        ]
    );
    // a page size of one forces the client to follow pagination links
    assert!(
        harness
            .api
            .borrow()
            .log
            .iter()
            .filter(|entry| *entry == "GET droplets")
            .count()
            >= 2
    );
}

#[test]
fn a_released_holding_records_the_time_after_the_provider_confirmed_its_droplet_gone() {
    let harness = harness();
    let owner = owner_tags("job_1", 1);
    let droplet = block_on(harness.provider.create(&owner, &droplet_spec("job-1-a"))).unwrap();
    harness
        .api
        .borrow_mut()
        .droplets
        .get_mut(&droplet.as_str().parse().unwrap())
        .unwrap()
        .reads_until_gone = Some(0);

    let policy = LeasePolicy::new(Duration::from_secs(120), Duration::from_secs(3_600)).unwrap();
    let lease = Lease::start(LeaseId::parse("lease_1").unwrap(), &policy, at(0), None).unwrap();
    let releasing = Holding::leased(owner.owner.clone(), owner.generation, lease)
        .request(DoKind::Droplet, at(0))
        .unwrap()
        .record_created(DoKind::Droplet, droplet, at(0))
        .unwrap()
        .expire(at(120_000))
        .unwrap();

    // each provider request takes a second, so the time read after the delete and the
    // confirming read is two seconds past the time before them
    let requests_before = harness.api.borrow().log.len();
    let clock = || {
        let requests = harness.api.borrow().log.len() - requests_before;
        at(120_000 + 1_000 * u64::try_from(requests).unwrap())
    };
    let reaped = block_on(reap(
        &harness.provider,
        &releasing,
        clock,
        &Backoff::default(),
    ))
    .unwrap();

    assert_eq!(
        reaped.holding.state(),
        &HoldingState::Released {
            reason: ReleaseReason::HeartbeatMissed,
            at: at(122_000),
        }
    );
}

#[test]
fn an_empty_tag_search_sent_before_the_settle_time_keeps_the_holding_releasing() {
    let harness = harness();
    let owner = owner_tags("job_1", 1);

    // the droplet's create was sent but its answer was lost, and it is not listed yet
    let policy = LeasePolicy::new(Duration::from_secs(120), Duration::from_secs(3_600)).unwrap();
    let lease = Lease::start(LeaseId::parse("lease_1").unwrap(), &policy, at(0), None).unwrap();
    let releasing = Holding::leased(owner.owner.clone(), owner.generation, lease)
        .request(DoKind::Droplet, at(0))
        .unwrap()
        .expire(at(120_000))
        .unwrap();

    // the search is sent a second before the two minute settle time ends and, since each
    // provider request takes a second, answers after it
    let requests_before = harness.api.borrow().log.len();
    let clock = || {
        let requests = harness.api.borrow().log.len() - requests_before;
        at(239_000 + 1_000 * u64::try_from(requests).unwrap())
    };
    let reaped = block_on(reap(
        &harness.provider,
        &releasing,
        clock,
        &Backoff::default(),
    ))
    .unwrap();

    assert!(harness.api.borrow().log.len() > requests_before);
    assert_eq!(
        reaped.holding.ready_deletes(),
        vec![DeleteTarget::Tagged {
            kind: DoKind::Droplet
        }]
    );
}

#[test]
fn an_expired_holding_finds_and_deletes_a_droplet_whose_create_answer_was_lost() {
    let harness = harness();
    let owner = owner_tags("job_1", 1);
    // the droplet was created but its response was lost, so the holding never recorded it
    let droplet = block_on(harness.provider.create(&owner, &droplet_spec("job-1-a"))).unwrap();

    let policy = LeasePolicy::new(Duration::from_secs(120), Duration::from_secs(3_600)).unwrap();
    let lease_id = LeaseId::parse("lease_1").unwrap();
    let lease = Lease::start(lease_id, &policy, at(0), None).unwrap();
    let holding = Holding::leased(owner.owner.clone(), owner.generation, lease)
        .request(DoKind::Droplet, at(0))
        .unwrap();
    let releasing = holding.expire(at(120_000)).unwrap();
    let backoff = Backoff::default();

    // the droplet is still visible right after the delete, so the pass schedules a retry
    let first = block_on(reap(
        &harness.provider,
        &releasing,
        || at(120_000),
        &backoff,
    ))
    .unwrap();
    assert!(first.errors.is_empty());
    assert!(matches!(
        first.holding.state(),
        HoldingState::Releasing { retry: Some(_), .. }
    ));
    assert_eq!(
        deletes(&harness),
        vec![format!("DELETE droplets/{droplet}")]
    );

    let second = block_on(reap(
        &harness.provider,
        &first.holding,
        || at(121_000),
        &backoff,
    ))
    .unwrap();
    assert_eq!(
        second.holding.state(),
        &HoldingState::Released {
            reason: ReleaseReason::HeartbeatMissed,
            at: at(121_000),
        }
    );
    assert_eq!(
        deletes(&harness),
        vec![
            format!("DELETE droplets/{droplet}"),
            format!("DELETE droplets/{droplet}"),
        ]
    );
    assert!(harness.api.borrow().droplets.is_empty());
}

#[test]
fn a_failing_provider_turns_into_a_retry_without_losing_targets() {
    let harness = harness();
    let owner = owner_tags("job_1", 1);
    let droplet = block_on(harness.provider.create(&owner, &droplet_spec("job-1-a"))).unwrap();
    harness.api.borrow_mut().failures.insert(
        format!("DELETE droplets/{droplet}"),
        vec![500, 500, 500, 500, 403],
    );

    let policy = LeasePolicy::new(Duration::from_secs(120), Duration::from_secs(3_600)).unwrap();
    let lease = Lease::start(LeaseId::parse("lease_1").unwrap(), &policy, at(0), None).unwrap();
    let releasing = Holding::leased(owner.owner.clone(), owner.generation, lease)
        .request(DoKind::Droplet, at(0))
        .unwrap()
        .record_created(DoKind::Droplet, droplet.clone(), at(0))
        .unwrap()
        .revoke(at(1_000));

    let backoff = Backoff::default();
    let failed = block_on(reap(&harness.provider, &releasing, || at(1_000), &backoff)).unwrap();
    assert_eq!(failed.errors.len(), 1);
    assert_eq!(failed.errors[0].code, ProviderErrorCode::ServerError);
    // the client retried the delete with backoff before giving up
    assert_eq!(harness.runtime.sleeps.borrow().len(), 3);
    assert_eq!(failed.holding.ready_deletes(), releasing.ready_deletes());

    let denied = block_on(reap(
        &harness.provider,
        &failed.holding,
        || at(5_000),
        &backoff,
    ))
    .unwrap();
    assert_eq!(denied.errors[0].code, ProviderErrorCode::PermissionDenied);
    let HoldingState::Releasing { retry, .. } = denied.holding.state() else {
        panic!("a failed cleanup keeps releasing");
    };
    assert_eq!(retry.map(|retry| retry.attempt), Some(2));
}

#[test]
fn creates_are_never_replayed_and_reads_retry() {
    let harness = harness();
    harness
        .api
        .borrow_mut()
        .failures
        .insert("POST droplets".to_owned(), vec![503]);
    let owner = owner_tags("job_1", 1);

    let error = block_on(harness.provider.create(&owner, &droplet_spec("job-1-a"))).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::ServerError);
    assert_eq!(
        harness
            .api
            .borrow()
            .log
            .iter()
            .filter(|entry| *entry == "POST droplets")
            .count(),
        1
    );

    harness
        .api
        .borrow_mut()
        .failures
        .insert("GET droplets".to_owned(), vec![429, 502]);
    let inventory = block_on(harness.provider.inventory()).unwrap();
    assert!(inventory.managed.is_empty());
    assert_eq!(harness.runtime.sleeps.borrow().len(), 2);
}

#[test]
fn pagination_cannot_leave_the_api_origin() {
    struct Hostile;
    impl HttpTransport for Hostile {
        fn send(
            &self,
            _request: HttpRequest,
        ) -> LocalFuture<'_, Result<HttpResponse, TransportError>> {
            Box::pin(async {
                Ok(respond(
                    200,
                    json!({
                        "droplets": [],
                        "links": { "pages": { "next": "https://evil.test/v2/droplets?page=2" } },
                    }),
                ))
            })
        }
    }

    let options = ClientOptions::new(DigitalOceanToken::parse("test-token").unwrap())
        .unwrap()
        .with_base_url(url::Url::parse(API).unwrap())
        .unwrap()
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        })
        .unwrap();
    let client = DigitalOceanClient::new(options, Hostile, FakeRuntime::default());

    let error = block_on(client.list_droplets_by_tag("cl-managed")).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::InvalidResponse);
}
