// End to end through a running payment-server. Setup uses the public API wherever one exists
// (register, wallets, deposits, FX rates, devices) and SQL only where an operator would
// (promoting the two admins, KYC level 2 — README "Try it" does the same).

use std::time::{Duration, Instant};

use api::devices::{MoneyMove, PaymentAuth};
use api::FeeConfig;
use base64::Engine;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::EncodePublicKey;
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::args::{Config, MixOp, Workload};
use crate::plan::{Planned, Rng};
use crate::stats::{Event, Op, Outcome, Posted, Step};
use crate::{par, Funding};

pub const PASSWORD: &str = "loadtest-password";
// TJS minor per USD minor, and back (10.93 TJS per dollar).
const USD_TJS: (i64, i64) = (1093, 100);
const SETUP_PARALLELISM: usize = 16;

pub struct User {
    pub id: Uuid,
    auth: String,
    pub wallet: Uuid,
    pub usd_wallet: Option<Uuid>,
    device: Option<(String, SigningKey)>,
}

pub struct Http {
    client: Client,
    base: String,
    pub users: Vec<User>,
    pub fees: FeeConfig,
    retries: u32,
}

enum Reply {
    Status(StatusCode, Value),
    Transport(String),
}

impl Reply {
    fn code(&self) -> String {
        match self {
            Reply::Transport(_) => "transport".into(),
            Reply::Status(s, body) => match body["error"]["code"].as_str() {
                Some(code) => format!("{} {code}", s.as_u16()),
                None => format!("{}", s.as_u16()),
            },
        }
    }

    fn expect(self, want: StatusCode, what: &str) -> Result<Value, String> {
        match self {
            Reply::Status(s, body) if s == want => Ok(body),
            Reply::Transport(e) => Err(format!("{what}: {e}")),
            other => Err(format!("{what}: expected {want}, got {}", other.code())),
        }
    }
}

fn client(concurrency: usize) -> Result<Client, String> {
    Client::builder()
        .pool_max_idle_per_host(concurrency + SETUP_PARALLELISM)
        .tcp_nodelay(true)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))
}

async fn send_once(rb: RequestBuilder) -> Reply {
    match rb.send().await {
        Ok(resp) => {
            let status = resp.status();
            match resp.bytes().await {
                Ok(b) => Reply::Status(status, serde_json::from_slice(&b).unwrap_or(Value::Null)),
                Err(e) => Reply::Transport(e.to_string()),
            }
        }
        Err(e) => Reply::Transport(e.to_string()),
    }
}

fn sign(key: &SigningKey, auth: &PaymentAuth<'_>) -> String {
    let sig: Signature = key.sign(&auth.payload());
    base64::engine::general_purpose::STANDARD.encode(sig.to_der().as_bytes())
}

impl Http {
    /// Resend the identical request (same Idempotency-Key, same signature) while the outcome
    /// is unknown, as a real client must.
    async fn send(&self, build: impl Fn() -> RequestBuilder) -> (Reply, u32) {
        let mut attempt = 0;
        loop {
            let reply = send_once(build()).await;
            let transient = match &reply {
                Reply::Transport(_) => true,
                Reply::Status(s, _) => matches!(s.as_u16(), 502..=504),
            };
            if !transient || attempt >= self.retries {
                return (reply, attempt);
            }
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(10 * attempt as u64)).await;
        }
    }

    fn money(reply: Reply, retries: u32, posted: Posted) -> Outcome {
        let (result, event) = match &reply {
            Reply::Status(s, _) if *s == StatusCode::CREATED || *s == StatusCode::OK => {
                (Ok(()), Event::Posted(posted))
            }
            Reply::Status(s, _) if s.is_client_error() => {
                (Err(reply.code()), Event::Rejected(posted.id))
            }
            _ => (Err(reply.code()), Event::Unresolved(posted)),
        };
        Outcome {
            result,
            retries,
            event: Some(event),
        }
    }

    fn read(reply: Reply, retries: u32) -> Outcome {
        Outcome {
            result: match &reply {
                Reply::Status(StatusCode::OK, _) => Ok(()),
                _ => Err(reply.code()),
            },
            retries,
            event: None,
        }
    }

    fn post(&self, user: &User, path: &str, key: Uuid, body: &Value) -> RequestBuilder {
        self.client
            .post(format!("{}{path}", self.base))
            .header("authorization", &user.auth)
            .header("idempotency-key", key.to_string())
            .json(body)
    }

    fn signed(rb: RequestBuilder, user: &User, auth: PaymentAuth<'_>) -> RequestBuilder {
        match &user.device {
            Some((id, key)) => rb
                .header("x-device-id", id)
                .header("x-device-signature", sign(key, &auth)),
            None => rb,
        }
    }

    fn net(&self, amount: i64) -> i64 {
        amount - self.fees.fee_minor(amount)
    }

    pub async fn exec(&self, planned: Planned, begin: Instant) -> Vec<Step> {
        let step = |op, started, outcome| Step {
            op,
            started,
            finished: Instant::now(),
            outcome,
        };
        match planned {
            Planned::Transfer { from, to, amount } => {
                let (u, v) = (&self.users[from], &self.users[to]);
                let key = Uuid::new_v4();
                let body = json!({"from_account": u.wallet, "to_account": v.wallet,
                                  "amount_minor": amount, "currency": "TJS"});
                let auth = PaymentAuth {
                    kind: MoneyMove::Transfer,
                    user_id: u.id,
                    idempotency_key: key,
                    from_account: Some(u.wallet),
                    to_account: v.wallet,
                    amount_minor: amount,
                    currency: "TJS",
                    check_id: None,
                };
                let rb = Self::signed(self.post(u, "/v1/transfers", key, &body), u, auth);
                let (reply, retries) = self.send(|| rb.try_clone().expect("buffered body")).await;
                let posted = Posted {
                    id: key,
                    legs: vec![(u.wallet, -amount), (v.wallet, self.net(amount))],
                };
                vec![step(
                    Op::Transfer,
                    begin,
                    Self::money(reply, retries, posted),
                )]
            }
            Planned::Balance { user } => {
                let u = &self.users[user];
                let url = format!("{}/v1/accounts/{}/balance", self.base, u.wallet);
                let (reply, retries) = self
                    .send(|| self.client.get(&url).header("authorization", &u.auth))
                    .await;
                vec![step(Op::Balance, begin, Self::read(reply, retries))]
            }
            Planned::Statement { user } => {
                let u = &self.users[user];
                let url = format!(
                    "{}/v1/accounts/{}/transactions?limit=20",
                    self.base, u.wallet
                );
                let (reply, retries) = self
                    .send(|| self.client.get(&url).header("authorization", &u.auth))
                    .await;
                vec![step(Op::Statement, begin, Self::read(reply, retries))]
            }
            Planned::Fx {
                user,
                to_usd,
                amount,
            } => {
                let u = &self.users[user];
                let usd = u.usd_wallet.expect("fx runs set up USD wallets");
                let (from, to, currency, amount) = if to_usd {
                    (u.wallet, usd, "TJS", amount)
                } else {
                    (usd, u.wallet, "USD", (amount / 100).max(1))
                };
                let key = Uuid::new_v4();
                let body = json!({"from_account": from, "to_account": to, "amount_minor": amount});
                let auth = PaymentAuth {
                    kind: MoneyMove::Fx,
                    user_id: u.id,
                    idempotency_key: key,
                    from_account: Some(from),
                    to_account: to,
                    amount_minor: amount,
                    currency,
                    check_id: None,
                };
                let rb = Self::signed(self.post(u, "/v1/fx", key, &body), u, auth);
                let (reply, retries) = self.send(|| rb.try_clone().expect("buffered body")).await;
                // The credited amount is the server's to compute; take it from the reply.
                let credited = match &reply {
                    Reply::Status(_, b) => b["credited_minor"].as_i64(),
                    Reply::Transport(_) => None,
                };
                let (rate_num, rate_den) = if to_usd {
                    (USD_TJS.1, USD_TJS.0)
                } else {
                    USD_TJS
                };
                let expected = (amount as i128 * rate_num as i128 / rate_den as i128) as i64;
                let posted = Posted {
                    id: key,
                    legs: vec![(from, -amount), (to, credited.unwrap_or(expected))],
                };
                vec![step(Op::Fx, begin, Self::money(reply, retries, posted))]
            }
            Planned::Check {
                merchant,
                payer,
                amount,
            } => {
                let (m, p) = (&self.users[merchant], &self.users[payer]);
                let check = Uuid::new_v4();
                let body =
                    json!({"account": m.wallet, "amount_minor": amount, "description": "loadtest"});
                let rb = self.post(m, "/v1/checks", check, &body);
                let (reply, retries) = self.send(|| rb.try_clone().expect("buffered body")).await;
                let created = matches!(reply, Reply::Status(StatusCode::CREATED, _));
                let result = if created { Ok(()) } else { Err(reply.code()) };
                let mut steps = vec![step(
                    Op::CheckCreate,
                    begin,
                    Outcome {
                        result,
                        retries,
                        event: None,
                    },
                )];
                if !created {
                    return steps;
                }
                let pay_begin = Instant::now();
                let key = Uuid::new_v4();
                let auth = PaymentAuth {
                    kind: MoneyMove::Check,
                    user_id: p.id,
                    idempotency_key: key,
                    from_account: Some(p.wallet),
                    to_account: m.wallet,
                    amount_minor: amount,
                    currency: "TJS",
                    check_id: Some(check),
                };
                let path = format!("/v1/checks/{check}/pay");
                let rb = Self::signed(
                    self.post(p, &path, key, &json!({"account": p.wallet})),
                    p,
                    auth,
                );
                let (reply, retries) = self.send(|| rb.try_clone().expect("buffered body")).await;
                let posted = Posted {
                    id: key,
                    legs: vec![(p.wallet, -amount), (m.wallet, self.net(amount))],
                };
                steps.push(step(
                    Op::CheckPay,
                    pay_begin,
                    Self::money(reply, retries, posted),
                ));
                steps
            }
        }
    }
}

// --- setup ---

async fn register(client: &Client, base: &str, phone: String) -> Result<(Uuid, String), String> {
    let reply = send_once(
        client
            .post(format!("{base}/v1/auth/register"))
            .json(&json!({"phone": phone, "password": PASSWORD})),
    )
    .await
    .expect(StatusCode::CREATED, &format!("register {phone}"))?;
    let id = reply["user_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or("register: no user_id")?;
    let token = reply["access_token"]
        .as_str()
        .ok_or("register: no access_token")?;
    Ok((id, format!("Bearer {token}")))
}

async fn tjs_wallet(client: &Client, base: &str, auth: &str) -> Result<Uuid, String> {
    let list = send_once(
        client
            .get(format!("{base}/v1/wallets"))
            .header("authorization", auth),
    )
    .await
    .expect(StatusCode::OK, "list wallets")?;
    list.as_array()
        .and_then(|ws| ws.iter().find(|w| w["currency"] == "TJS"))
        .and_then(|w| w["id"].as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| "registration created no TJS wallet".to_string())
}

/// Posts a deposit with whichever control the server runs: 201 = single admin, 202 = needs
/// the second admin's approval.
async fn deposit(
    client: &Client,
    base: &str,
    admins: &(String, String),
    wallet: Uuid,
    amount: i64,
    currency: &str,
) -> Result<Funding, String> {
    let key = Uuid::new_v4();
    let reply = send_once(
        client
            .post(format!("{base}/v1/deposits"))
            .header("authorization", &admins.0)
            .header("idempotency-key", key.to_string())
            .json(&json!({"user_account": wallet, "amount_minor": amount, "currency": currency})),
    )
    .await;
    match reply {
        Reply::Status(StatusCode::CREATED, _) => {}
        Reply::Status(StatusCode::ACCEPTED, _) => {
            let approved = send_once(
                client
                    .post(format!("{base}/v1/admin/deposits/{key}/approve"))
                    .header("authorization", &admins.1),
            )
            .await
            .expect(StatusCode::OK, "approve deposit")?;
            if approved["status"] != "posted" {
                return Err(format!("deposit approval did not post: {approved}"));
            }
        }
        other => return Err(format!("deposit: {}", other.code())),
    }
    Ok(Funding {
        id: key,
        wallet,
        amount,
    })
}

pub async fn setup(
    cfg: &Config,
    base: String,
    pool: &PgPool,
    rng: &mut Rng,
) -> Result<(Http, Vec<Funding>), String> {
    if cfg.users > 999_000 {
        return Err("http mode supports at most 999000 users".into());
    }
    let client = client(cfg.concurrency)?;
    let tag = rng.below(100_000);
    let phone = |i: usize| format!("9{tag:05}{i:06}");

    let a = register(&client, &base, phone(999_998)).await?;
    let b = register(&client, &base, phone(999_999)).await?;
    sqlx::query("UPDATE users SET is_admin = true WHERE id = ANY($1)")
        .bind(vec![a.0, b.0])
        .execute(pool)
        .await
        .map_err(|e| format!("promote admins: {e}"))?;
    let admins = (a.1, b.1);

    eprintln!("registering {} users (Argon2-bound)...", cfg.users);
    let (c, bs) = (client.clone(), base.clone());
    let registered = par(SETUP_PARALLELISM, (0..cfg.users).collect(), move |i| {
        let (client, base) = (c.clone(), bs.clone());
        async move {
            let (id, auth) = register(&client, &base, format!("9{tag:05}{i:06}")).await?;
            let wallet = tjs_wallet(&client, &base, &auth).await?;
            Ok((id, auth, wallet))
        }
    })
    .await?;
    let ids: Vec<Uuid> = registered.iter().map(|r| r.0).collect();
    sqlx::query("UPDATE users SET kyc_level = 2 WHERE id = ANY($1)")
        .bind(&ids)
        .execute(pool)
        .await
        .map_err(|e| format!("set KYC level: {e}"))?;

    let fx = cfg.mix.has(MixOp::Fx);
    let usd_wallets: Vec<Option<Uuid>> = if fx {
        send_once(
            client
                .post(format!("{base}/v1/admin/fx-rates"))
                .header("authorization", &admins.0)
                .json(
                    &json!({"base": "USD", "quote": "TJS", "rate_num": USD_TJS.0,
                              "rate_den": USD_TJS.1, "also_reverse": true}),
                ),
        )
        .await
        .expect(StatusCode::NO_CONTENT, "set FX rate")?;
        let (c, bs) = (client.clone(), base.clone());
        let auths: Vec<String> = registered.iter().map(|r| r.1.clone()).collect();
        par(SETUP_PARALLELISM, auths, move |auth| {
            let (client, base) = (c.clone(), bs.clone());
            async move {
                let w = send_once(
                    client
                        .post(format!("{base}/v1/wallets"))
                        .header("authorization", &auth)
                        .json(&json!({"currency": "USD"})),
                )
                .await
                .expect(StatusCode::CREATED, "create USD wallet")?;
                w["id"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .map(Some)
                    .ok_or_else(|| "USD wallet without id".to_string())
            }
        })
        .await?
    } else {
        vec![None; cfg.users]
    };

    // One deposit per wallet; the contention payer gets enough for the whole run.
    let mut deposits: Vec<(Uuid, i64, &'static str)> = Vec::new();
    for (i, r) in registered.iter().enumerate() {
        let n = if cfg.workload == Workload::Contention && i == 0 {
            50
        } else {
            1
        };
        deposits.extend(std::iter::repeat_n((r.2, cfg.fund, "TJS"), n));
    }
    for w in usd_wallets.iter().flatten() {
        deposits.push((*w, cfg.fund / 10, "USD"));
    }
    eprintln!("funding {} deposits...", deposits.len());
    let (c, bs, ad) = (client.clone(), base.clone(), admins.clone());
    let funding = par(
        SETUP_PARALLELISM,
        deposits,
        move |(wallet, amount, currency)| {
            let (client, base, admins) = (c.clone(), bs.clone(), ad.clone());
            async move { deposit(&client, &base, &admins, wallet, amount, currency).await }
        },
    )
    .await?;

    let devices: Vec<Option<(String, SigningKey)>> = if cfg.sign {
        eprintln!("registering {} devices (Argon2-bound)...", cfg.users);
        let (c, bs) = (client.clone(), base.clone());
        let auths: Vec<String> = registered.iter().map(|r| r.1.clone()).collect();
        par(SETUP_PARALLELISM, auths, move |auth| {
            let (client, base) = (c.clone(), bs.clone());
            async move {
                let key = SigningKey::random(&mut rand_core::OsRng);
                let spki = p256::PublicKey::from(key.verifying_key())
                    .to_public_key_der()
                    .map_err(|e| format!("encode device key: {e}"))?;
                let b64 = base64::engine::general_purpose::STANDARD.encode(spki.as_bytes());
                let dev = send_once(
                    client
                        .post(format!("{base}/v1/devices"))
                        .header("authorization", &auth)
                        .json(
                            &json!({"public_key": b64, "label": "loadtest", "password": PASSWORD}),
                        ),
                )
                .await
                .expect(StatusCode::CREATED, "register device")?;
                let id = dev["id"].as_str().ok_or("device without id")?.to_string();
                Ok(Some((id, key)))
            }
        })
        .await?
    } else {
        (0..cfg.users).map(|_| None).collect()
    };

    let users: Vec<User> = registered
        .into_iter()
        .zip(usd_wallets)
        .zip(devices)
        .map(|(((id, auth, wallet), usd_wallet), device)| User {
            id,
            auth,
            wallet,
            usd_wallet,
            device,
        })
        .collect();

    let fees = if cfg.server_bin.is_some() {
        FeeConfig {
            transfer_bps: cfg.fee_bps,
        }
    } else {
        let conf = send_once(
            client
                .get(format!("{base}/v1/config"))
                .header("authorization", &users[0].auth),
        )
        .await
        .expect(StatusCode::OK, "read /v1/config")?;
        FeeConfig {
            transfer_bps: conf["transfer_fee_bps"]
                .as_u64()
                .ok_or("config without transfer_fee_bps")? as u32,
        }
    };

    Ok((
        Http {
            client,
            base,
            users,
            fees,
            retries: cfg.retries,
        },
        funding,
    ))
}
