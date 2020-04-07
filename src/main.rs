use deadpool_postgres::{Manager, Pool};
use futures_util::{
    future,
    future::{ TryFutureExt},
    stream::{self, StreamExt},
};
use serde::Deserialize;
use slog_scope::{crit, error};
use std::{convert::TryFrom, sync::Arc};
use tgcd::{
    raw::{tgcd_server, AddTags, GetMultipleTagsReq, GetMultipleTagsResp, Hash, SrcDest, Tags},
    Blake2bHash, HashError, Tag, TagError,
};
use thiserror::Error;
use tokio::signal::unix::{signal, SignalKind};
use tokio_postgres as postgres;
use tonic::{transport::Server, Request, Response, Status};

#[derive(Deserialize)]
struct Config {
    postgres_url: String,

    #[serde(default = "port")]
    port: u16,

    #[serde(default = "pool_size")]
    pool_size: usize,
}

fn port() -> u16 {
    8080
}

fn pool_size() -> usize {
    16
}

#[derive(Clone)]
struct Tgcd {
    inner: Arc<TgcdInner>,
}

mod embedded {
    refinery::embed_migrations!("sql");
}

impl Tgcd {
    async fn new(cfg: &Config) -> Result<Self, SetupError> {
        // TODO: remove unwrap
        let postgres_config: postgres::config::Config = cfg.postgres_url.parse().unwrap();

        let (mut client, pg) = postgres_config
            .connect(postgres::NoTls)
            .await
            .map_err(SetupError::PostgresConnect)?;
        // after client is dropped the pg future also terminates
        // so this doesn't leak
        tokio::task::spawn(pg);
        embedded::migrations::runner()
            .run_async(&mut client)
            .await?;

        let mngr = Manager::new(postgres_config, postgres::NoTls);
        let pool = Pool::new(mngr, cfg.pool_size);

        Ok(Self {
            inner: Arc::new(TgcdInner { pool }),
        })
    }
}

struct TgcdInner {
    pool: Pool,
}

#[derive(Error, Debug)]
pub enum SetupError {
    #[error("Can't connect to postgres: {0}")]
    PostgresConnect(#[source] postgres::Error),

    #[error("Failed creating schema: {0}")]
    PostgresSchema(
        #[source]
        #[from]
        refinery_migrations::Error,
    ),

    #[error("Missing environment variable: {0}")]
    Env(#[from] envy::Error),

    #[error("Can't bind server: {0}")]
    Bind(#[from] tonic::transport::Error),

    #[error("Can't register signal: {0}")]
    Signal(std::io::Error),
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Error from postgres: {0}")]
    Postgres(#[from] postgres::Error),

    #[error("Invalid hash: {0}")]
    ArgHash(HashError),

    #[error("Invalid tag: {0}")]
    ArgTag(TagError),
}

impl From<Error> for Status {
    fn from(other: Error) -> Self {
        match other {
            Error::Postgres(e) => {
                error!("Db error"; slog::o!("error" => e.to_string()));
                Status::new(tonic::Code::Unavailable, "db error")
            }
            Error::ArgHash(_) | Error::ArgTag(_) => {
                Status::new(tonic::Code::InvalidArgument, "Received invalid argument")
            }
        }
    }
}

async fn get_tags(client: &postgres::Client, hash: &Blake2bHash) -> Result<Vec<String>, Error> {
    let stmnt = client
        .prepare(
            "
        SELECT tag.name
        FROM tag tag, hash_tag hash_tag, hash hash
        WHERE
            tag.id = hash_tag.tag_id
            AND hash_tag.hash_id = hash.id
            AND hash.hash = $1",
        )
        .await?;
    let tags = client.query(&stmnt, &[&hash.as_ref()]).await?;
    Ok(tags.into_iter().map(|row| row.get(0)).collect())
}

async fn get_or_insert_hash(
    client: &postgres::Transaction<'_>,
    hash: &Blake2bHash,
) -> Result<i32, Error> {
    let stmnt = client
        .prepare(
            "
    WITH inserted AS (
        INSERT INTO hash(hash)
        VALUES($1)
        ON CONFLICT DO NOTHING
        RETURNING id
    )
    SELECT * FROM inserted

    UNION ALL

    SELECT id FROM hash
    WHERE hash = $1
    ",
        )
        .await?;

    let row = client.query_one(&stmnt, &[&hash.as_ref()]).await?;

    Ok(row.get(0))
}

async fn get_or_insert_tag(txn: &postgres::Transaction<'_>, tag: &str) -> Result<i32, Error> {
    let stmnt = txn
        .prepare(
            "
    WITH inserted AS (
        INSERT INTO tag(name)
        VALUES($1)
        ON CONFLICT DO NOTHING
        RETURNING id
    )
    SELECT * FROM inserted

    UNION ALL

    SELECT id FROM tag
    WHERE name = $1
    ",
        )
        .await?;

    let row = txn.query_one(&stmnt, &[&tag]).await?;

    Ok(row.get(0))
}

async fn add_tags_to_hash(
    txn: &postgres::Transaction<'_>,
    hash: &Blake2bHash,
    tags: &[Tag],
) -> Result<(), Error> {
    let hash_id = get_or_insert_hash(&txn, &hash).await?;
    for tag in tags {
        let tag_id = get_or_insert_tag(&txn, &tag).await?;
        txn.execute(
            "INSERT INTO hash_tag(tag_id, hash_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&tag_id, &hash_id],
        )
        .await?;
    }
    Ok(())
}

#[tonic::async_trait]
impl tgcd_server::Tgcd for Tgcd {
    async fn get_tags(&self, req: Request<Hash>) -> Result<Response<Tags>, Status> {
        let client = self.inner.pool.get().await.unwrap();
        let hash = Blake2bHash::try_from(&*req.into_inner().hash).map_err(Error::ArgHash)?;
        let tags = get_tags(&client, &hash).await?;

        Ok(Response::new(Tags { tags }))
    }

    async fn add_tags_to_hash(&self, req: Request<AddTags>) -> Result<Response<()>, Status> {
        let mut client = self.inner.pool.get().await.unwrap();
        let AddTags { hash, tags } = req.into_inner();
        let hash = Blake2bHash::try_from(&*hash).map_err(Error::ArgHash)?;
        let tags = tags
            .into_iter()
            .map(Tag::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::ArgTag)?;

        let txn = client.transaction().map_err(Error::Postgres).await?;
        add_tags_to_hash(&txn, &hash, &tags).await?;

        txn.commit().map_err(Error::Postgres).await?;

        Ok(Response::new(()))
    }

    async fn get_multiple_tags(
        &self,
        req: Request<GetMultipleTagsReq>,
    ) -> Result<Response<GetMultipleTagsResp>, Status> {
        let client = self.inner.pool.get().await.unwrap();
        let hashes = req.into_inner().hashes;
        let hashes = hashes
            .into_iter()
            .map(|hash| Blake2bHash::try_from(&*hash))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::ArgHash)?;

        let tags = future::try_join_all(
            hashes
                .iter()
                .map(|hash| get_tags(&client, &hash).map_ok(|tags| Tags { tags })),
        )
        .await?;

        Ok(Response::new(GetMultipleTagsResp { tags }))
    }

    async fn copy_tags(&self, req: Request<SrcDest>) -> Result<Response<()>, Status> {
        let SrcDest {
            src_hash,
            dest_hash,
        } = req.into_inner();
        let mut client = self.inner.pool.get().await.unwrap();

        let src_hash = Blake2bHash::try_from(&*src_hash).map_err(Error::ArgHash)?;
        let dest_hash = Blake2bHash::try_from(&*dest_hash).map_err(Error::ArgHash)?;

        let src_tags = get_tags(&client, &src_hash)
            .await?
            .into_iter()
            .map(|a| Tag::try_from(a).unwrap())
            .collect::<Vec<_>>();

        let txn = client.transaction().await.map_err(Error::Postgres)?;
        add_tags_to_hash(&txn, &dest_hash, &src_tags).await?;
        txn.commit().await.map_err(Error::Postgres)?;

        Ok(Response::new(()))
    }
}

async fn run() -> Result<(), SetupError> {
    let config: Config = envy::from_env()?;
    let tgcd = Tgcd::new(&config).await?;

    let signals = [SignalKind::interrupt(), SignalKind::terminate()]
        .iter()
        .map(|&signo| signal(signo))
        .collect::<Result<Vec<_>, _>>()
        .map_err(SetupError::Signal)?;
    let terminate = async move {
        stream::select_all(signals).next().await;
    };

    Server::builder()
        .add_service(tgcd_server::TgcdServer::new(tgcd))
        .serve_with_shutdown(([0, 0, 0, 0], config.port).into(), terminate)
        .await?;

    Ok(())
}

fn main() {
    use slog::Drain;
    let decorator = slog_term::TermDecorator::new().build();
    // FIXME: use async logger instead
    let drain = std::sync::Mutex::new(slog_term::FullFormat::new(decorator).build())
        .filter_level(slog::Level::Info)
        .fuse();

    let logger = slog::Logger::root(drain, slog::o!());

    let _scope_guard = slog_scope::set_global_logger(logger);

    let mut rt = tokio::runtime::Builder::new()
        .threaded_scheduler()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();
    if let Err(e) = rt.block_on(run()) {
        crit!("{}", e);
        std::process::exit(1);
    }
}
