use std::{collections::HashMap, path::PathBuf};

use toy_farm_compiler::Compiler;
use toy_farm_core::{
    config_regex::ConfigRegex, persistent_cache::PersistentCacheConfig, Config, Mode,
};

pub async fn create_compiler(
    input: HashMap<String, String>,
    cwd: PathBuf,
    _crate_path: PathBuf,
    _minify: bool,
) -> Compiler {
    Compiler::new(Config {
        input,
        root: cwd.to_string_lossy().to_string(),
        // runtime: generate_runtime(crate_path),
        output: Default::default(),
        persistent_cache: Box::new(PersistentCacheConfig::Bool(false)),
        mode: Mode::Development,
        record: false,
        external: vec![
            ConfigRegex::new("^react-refresh$"),
            ConfigRegex::new("^module$"),
            ConfigRegex::new("^vue$"),
        ],
        // sourcemap: SourcemapConfig::Bool(false),
        // lazy_compilation: false,
        // progress: false,
        // minify: Box::new(BoolOrObj::from(minify)),
        // preset_env: Box::new(PresetEnvConfig::Bool(false)),
        ..Default::default()
    })
    .await
}
