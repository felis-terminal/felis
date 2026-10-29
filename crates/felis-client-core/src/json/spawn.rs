//! DTO for [`SpawnArgs`], what a `create` names.

use felis_protocol::messages::SpawnArgs;

use super::common::{Base64, RequestedDimsJson};

json_dto! {
    #[non_exhaustive]
    pub struct EnvPairJson {
        pub name: String,
        pub value: String,
    }

    /// One entry of a child's base environment, as raw platform bytes.
    #[non_exhaustive]
    pub struct EnvBytesJson {
        pub name: Base64,
        pub value: Base64,
    }

    /// Every key may be absent, which reads as its default.
    #[derive(Default)]
    #[serde(default)]
    #[non_exhaustive]
    pub struct SpawnArgsJson {
        /// Empty means the daemon's default (`$SHELL`).
        pub command: String,
        pub args: Vec<String>,
        /// Empty means the daemon's cwd.
        pub cwd: String,
        pub env: Vec<EnvPairJson>,
        /// `null` asks for the daemon default geometry.
        pub dims: Option<RequestedDimsJson>,
        /// `null` inherits the daemon's environment.
        pub env_base: Option<Vec<EnvBytesJson>>,
        pub tags: Vec<String>,
    }
}

impl From<(String, String)> for EnvPairJson {
    fn from((name, value): (String, String)) -> Self {
        Self { name, value }
    }
}

impl From<EnvPairJson> for (String, String) {
    fn from(pair: EnvPairJson) -> Self {
        (pair.name, pair.value)
    }
}

impl From<(Vec<u8>, Vec<u8>)> for EnvBytesJson {
    fn from((name, value): (Vec<u8>, Vec<u8>)) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

impl From<EnvBytesJson> for (Vec<u8>, Vec<u8>) {
    fn from(pair: EnvBytesJson) -> Self {
        (pair.name.0, pair.value.0)
    }
}

impl From<SpawnArgs> for SpawnArgsJson {
    fn from(args: SpawnArgs) -> Self {
        Self {
            command: args.command,
            args: args.args,
            cwd: args.cwd,
            env: args.env.into_iter().map(Into::into).collect(),
            dims: args.dims.map(Into::into),
            env_base: args
                .env_base
                .map(|env| env.into_iter().map(Into::into).collect()),
            tags: args.tags,
        }
    }
}

impl From<SpawnArgsJson> for SpawnArgs {
    fn from(args: SpawnArgsJson) -> Self {
        Self {
            command: args.command,
            args: args.args,
            cwd: args.cwd,
            env: args.env.into_iter().map(Into::into).collect(),
            dims: args.dims.map(Into::into),
            env_base: args
                .env_base
                .map(|env| env.into_iter().map(Into::into).collect()),
            tags: args.tags,
        }
    }
}
