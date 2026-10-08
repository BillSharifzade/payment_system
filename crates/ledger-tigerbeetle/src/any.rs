use std::sync::Arc;

use crate::client::{
    Account, AccountResult, ClientError, QueryFilter, TbClient, Transfer, TransferResult,
};
use crate::sim::SimTb;

/// The cluster a process talks to: the native client (feature `native-client`), or the
/// in-process model, which test harnesses use to run whole services against the protocol
/// without a cluster (and to inject faults). Nothing selects `Sim` from the environment.
#[derive(Clone)]
pub enum AnyTb {
    #[cfg(feature = "native-client")]
    Live(Arc<crate::native::LiveTb>),
    Sim(Arc<SimTb>),
}

/// What a server or worker runs on, per `LEDGER_BACKEND` and `TIGERBEETLE_*` (strictly parsed,
/// before anything connects): `None` for `postgres`. A build without the `native-client`
/// feature refuses `tigerbeetle` here.
pub fn backend_from_env() -> Result<Option<crate::TbConfig>, String> {
    match crate::Backend::from_env()? {
        crate::Backend::Postgres => Ok(None),
        crate::Backend::TigerBeetle if cfg!(feature = "native-client") => {
            Ok(Some(crate::TbConfig::from_env()?))
        }
        crate::Backend::TigerBeetle => Err(NO_CLIENT.into()),
    }
}

const NO_CLIENT: &str = "LEDGER_BACKEND=tigerbeetle needs a build with `--features tigerbeetle` \
                         (this one has no TigerBeetle client)";

impl crate::HybridLedger<AnyTb> {
    /// The native client, with the cluster [prepared](crate::HybridLedger::prepare) to serve.
    pub async fn connect(
        cfg: crate::TbConfig,
        pg: storage::PostgresLedger,
    ) -> Result<Self, String> {
        let ledger = Self::live(cfg, pg)?;
        ledger
            .prepare()
            .await
            .map_err(|e| format!("LEDGER_BACKEND=tigerbeetle: {e}"))?;
        Ok(ledger)
    }

    /// The native client, unprepared (the cut-over commands).
    #[cfg(feature = "native-client")]
    pub fn live(cfg: crate::TbConfig, pg: storage::PostgresLedger) -> Result<Self, String> {
        let client = crate::native::LiveTb::from_config(&cfg)
            .map_err(|e| format!("TigerBeetle client: {e}"))?;
        Ok(Self::new(
            crate::TbLedger::new(AnyTb::Live(Arc::new(client)), cfg),
            pg,
        ))
    }

    #[cfg(not(feature = "native-client"))]
    pub fn live(_: crate::TbConfig, _: storage::PostgresLedger) -> Result<Self, String> {
        Err(NO_CLIENT.into())
    }
}

macro_rules! dispatch {
    ($self:ident, $c:ident => $call:expr) => {
        match $self {
            #[cfg(feature = "native-client")]
            AnyTb::Live($c) => $call.await,
            AnyTb::Sim($c) => $call.await,
        }
    };
}

impl TbClient for AnyTb {
    async fn create_accounts(
        &self,
        accounts: Vec<Account>,
    ) -> Result<Vec<AccountResult>, ClientError> {
        dispatch!(self, c => c.create_accounts(accounts))
    }

    async fn create_transfers(
        &self,
        transfers: Vec<Transfer>,
    ) -> Result<Vec<TransferResult>, ClientError> {
        dispatch!(self, c => c.create_transfers(transfers))
    }

    async fn lookup_accounts(&self, ids: Vec<u128>) -> Result<Vec<Account>, ClientError> {
        dispatch!(self, c => c.lookup_accounts(ids))
    }

    async fn lookup_transfers(&self, ids: Vec<u128>) -> Result<Vec<Transfer>, ClientError> {
        dispatch!(self, c => c.lookup_transfers(ids))
    }

    async fn query_transfers(&self, filter: QueryFilter) -> Result<Vec<Transfer>, ClientError> {
        dispatch!(self, c => c.query_transfers(filter))
    }

    fn cluster_id(&self) -> u128 {
        match self {
            #[cfg(feature = "native-client")]
            AnyTb::Live(c) => c.cluster_id(),
            AnyTb::Sim(c) => c.cluster_id(),
        }
    }
}
