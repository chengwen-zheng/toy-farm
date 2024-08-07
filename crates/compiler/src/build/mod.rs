mod load;
mod module_cached;
mod parse;
mod resolve;
mod transform;
use std::sync::Arc;

use load::load;
use parse::parse;
use resolve::resolve;
use transform::transform;

use crate::Compiler;
use futures::future::join_all;

use module_cached::{
    get_content_hash_of_module, get_timestamp_of_module, try_get_module_cache_by_hash,
    try_get_module_cache_by_timestamp,
};
use tokio::{
    sync::mpsc::{channel, Receiver, Sender},
    task::JoinHandle,
};
use toy_farm_core::{
    error::Result, module::ModuleId, module_cache::CachedModule, plugin::PluginResolveHookResult,
    plugin_driver::PluginDriverTransformHookResult, CompilationContext, CompilationError, Module,
    ModuleGraph, ModuleGraphEdgeDataItem, ModuleMetaData, ModuleType,
    PluginAnalyzeDepsHookResultEntry, PluginLoadHookParam, PluginParseHookParam,
    PluginProcessModuleHookParam, PluginResolveHookParam, PluginTransformHookParam, ResolveKind,
};

use toy_farm_utils::stringify_query;
#[derive(Debug)]
pub(crate) struct ResolveModuleIdResult {
    pub module_id: ModuleId,
    pub resolve_result: PluginResolveHookResult,
}
pub(crate) struct ResolvedModuleInfo {
    pub module: Module,
    pub resolve_module_id_result: ResolveModuleIdResult,
}

enum ResolveModuleResult {
    // The module is already built
    Built(ModuleId),
    Cached(ModuleId),
    Success(Box<ResolvedModuleInfo>),
}

pub(crate) struct BuildModuleGraphParams {
    pub resolve_param: PluginResolveHookParam,
    pub context: Arc<CompilationContext>,
    pub cached_dependency: Option<ModuleId>,
    pub order: usize,
    pub err_sender: Sender<CompilationError>,
}
pub(crate) struct HandleDependenciesParams {
    pub module: Module,
    pub resolve_param: PluginResolveHookParam,
    pub order: usize,
    pub deps: Vec<(PluginAnalyzeDepsHookResultEntry, Option<ModuleId>)>,
    // pub thread_pool: Arc<ThreadPool>,
    pub err_sender: Sender<CompilationError>,
    pub context: Arc<CompilationContext>,
}

use self::module_cached::handle_cached_modules;

macro_rules! call_and_catch_error {
    ($func:ident, $param:expr, $context:expr) => {
        match $func($param, $context).await {
            Ok(result) => result,
            Err(e) => {
                return Err(e);
            }
        }
    };
    () => {};
}

impl Compiler {
    // MARK: RESOLVE MODULE ID
    async fn resolve_module_id(
        resolve_param: &PluginResolveHookParam,
        context: &Arc<CompilationContext>,
    ) -> Result<ResolveModuleIdResult> {
        let get_module_id = |resolve_result: &PluginResolveHookResult| {
            // make query part of module id
            ModuleId::new(
                &resolve_result.resolved_path,
                &stringify_query(&resolve_result.query),
            )
        };

        // MARK: RESOLVE
        let resolve_result = match resolve(resolve_param.clone(), context.clone()).await {
            Ok(result) => result,
            Err(_) => {
                // log error
                return Err(CompilationError::GenericError(
                    "Failed to resolve module id".to_string(),
                ));
            }
        };

        let module_id = get_module_id(&resolve_result);

        Ok(ResolveModuleIdResult {
            module_id,
            resolve_result,
        })
    }

    // MARK: BUILD
    pub async fn build(&self) {
        let (err_sender, _err_receiver) = Self::create_thread_channel();

        for (order, (name, source)) in self.context.config.input.iter().enumerate() {
            println!("Index: {}, Name: {}, Source: {}", order, name, source);

            let resolve_param = PluginResolveHookParam {
                kind: ResolveKind::Entry(name.clone()),
                source: source.clone(),
                importer: None,
            };

            let build_module_graph_params = BuildModuleGraphParams {
                resolve_param,
                context: self.context.clone(),
                cached_dependency: None,
                order,
                err_sender: err_sender.clone(),
            };

            Compiler::build_module_graph(build_module_graph_params).await;
        }
    }

    pub(crate) fn create_module(module_id: ModuleId, external: bool, immutable: bool) -> Module {
        let mut module = Module::new(module_id);

        // if the module is external, return a external module
        if external {
            module.external = true;
        }

        if immutable {
            module.immutable = true;
        }

        module
    }

    pub(crate) fn insert_dummy_module(module_id: &ModuleId, module_graph: &mut ModuleGraph) {
        // insert a dummy module to the graph to prevent the module from being handled twice
        module_graph.add_module(Compiler::create_module(module_id.clone(), false, false));
    }

    // MARK: BUILD MODULE GRAPH
    async fn build_module_graph(params: BuildModuleGraphParams) {
        // build module graph
        let BuildModuleGraphParams {
            resolve_param,
            context,
            cached_dependency,
            order,
            err_sender,
        } = params;

        let resolve_module_result =
            match resolve_module(&resolve_param, cached_dependency, &context).await {
                Ok(result) => result,
                Err(e) => {
                    // log error
                    err_sender.send(e).await.unwrap();
                    return;
                }
            };

        match resolve_module_result {
            ResolveModuleResult::Success(resolved_module_info) => {
                let ResolvedModuleInfo {
                    mut module,
                    resolve_module_id_result,
                } = *resolved_module_info;
                if resolve_module_id_result.resolve_result.external {
                    // insert external module to the graph
                    let module_id = module.id.clone();
                    Self::add_module(module, &resolve_param.kind, &context).await;
                    Self::add_edge(&resolve_param, module_id, order, &context).await;
                    return;
                }

                let context_clone = Arc::clone(&context);

                // handle the resolved module
                match Self::build_module(
                    resolve_module_id_result.resolve_result,
                    &mut module,
                    context,
                )
                .await
                {
                    Err(e) => {
                        err_sender.send(e).await.unwrap();
                    }
                    Ok(deps) => {
                        let params = HandleDependenciesParams {
                            module,
                            resolve_param,
                            order,
                            deps,
                            err_sender,
                            context: context_clone,
                        };
                        handle_dependencies(params).await;
                    }
                }
            }
            ResolveModuleResult::Built(module_id) => {
                // handle the built module
                Self::add_edge(&resolve_param, module_id, order, &context).await;
            }
            ResolveModuleResult::Cached(module_id) => {
                // handle the cached module
                let mut cached_module = context.cache_manager.module_cache.get_cache(&module_id);
                if let Err(e) = handle_cached_modules(&mut cached_module, &context).await {
                    err_sender.send(e).await.unwrap();
                };

                let params = HandleDependenciesParams {
                    module: cached_module.module,
                    resolve_param,
                    order,
                    deps: CachedModule::dep_sources(cached_module.dependencies),
                    // err_sender,
                    context,
                    err_sender,
                };

                handle_dependencies(params).await;
            }
        }
    }

    async fn add_edge(
        resolve_param: &PluginResolveHookParam,
        module_id: ModuleId,
        order: usize,
        context: &CompilationContext,
    ) {
        let mut module_graph = context.module_graph.write().await;
        if let Some(importer_id) = &resolve_param.importer {
            module_graph.add_edge_item(
              importer_id,
              &module_id,
              ModuleGraphEdgeDataItem {
                source: resolve_param.source.clone(),
                kind: resolve_param.kind.clone(),
                order,
              },
            ).expect("failed to add edge to the module graph, the endpoint modules of the edge should be in the graph")
        }
    }

    /// add a module to the module graph, if the module already exists, update it
    pub(crate) async fn add_module(
        module: Module,
        kind: &ResolveKind,
        context: &CompilationContext,
    ) {
        let mut module_graph = context.module_graph.write().await;

        // mark entry module
        if let ResolveKind::Entry(name) = kind {
            module_graph
                .entries
                .insert(module.id.clone(), name.to_string());
        }

        // check if the module already exists
        if module_graph.has_module(&module.id) {
            module_graph.replace_module(module);
        } else {
            module_graph.add_module(module);
        }
    }

    pub(crate) fn create_thread_channel() -> (Sender<CompilationError>, Receiver<CompilationError>)
    {
        let (err_sender, err_receiver) = channel::<CompilationError>(1024);

        (err_sender, err_receiver)
    }

    /// Resolving, loading, transforming and parsing a module, return the module and its dependencies if success
    pub(crate) async fn build_module(
        resolve_result: PluginResolveHookResult,
        module: &mut Module,
        context: Arc<CompilationContext>,
    ) -> Result<Vec<(PluginAnalyzeDepsHookResultEntry, Option<ModuleId>)>> {
        let context_clone = Arc::clone(&context);

        module.last_update_timestamp = if module.immutable {
            0
        } else {
            get_timestamp_of_module(&module.id, &context.config.root)
        };

        if let Some(cached_module) = try_get_module_cache_by_timestamp(
            &module.id,
            module.last_update_timestamp,
            context_clone,
        )
        .await?
        {
            *module = cached_module.module;
            return Ok(CachedModule::dep_sources(cached_module.dependencies));
        }

        // MARK: LOAD
        let load_param = PluginLoadHookParam {
            resolved_path: resolve_result.resolved_path.clone(),
            query: resolve_result.query.clone(),
            meta: resolve_result.meta.clone(),
            module_id: module.id.to_string(),
        };

        let load_result = call_and_catch_error!(load, Arc::new(load_param), Arc::clone(&context));
        let mut source_map_chain = vec![];

        if let Some(source_map) = load_result.source_map {
            source_map_chain.push(Arc::new(source_map));
        }

        let load_module_type = load_result.module_type.clone();
        let transform_param = PluginTransformHookParam {
            content: load_result.content,
            resolved_path: resolve_result.resolved_path.clone(),
            module_type: load_module_type.clone(),
            query: resolve_result.query.clone(),
            meta: resolve_result.meta.clone(),
            module_id: module.id.to_string(),
            source_map_chain,
        };

        let transform_result = call_and_catch_error!(transform, transform_param, context.clone());

        module.content = Arc::new(transform_result.content.clone());

        module.content_hash = if module.immutable {
            "immutable_module".to_string()
        } else {
            get_content_hash_of_module(&transform_result.content)
        };

        if let Some(cached_module) =
            try_get_module_cache_by_hash(&module.id, &module.content_hash, &context.clone()).await?
        {
            *module = cached_module.module;
            return Ok(CachedModule::dep_sources(cached_module.dependencies));
        }

        let deps = Self::build_module_after_transform(
            resolve_result,
            load_module_type,
            transform_result,
            module,
            &context,
        )
        .await?;

        Ok(deps.into_iter().map(|dep| (dep, None)).collect())
    }

    async fn build_module_after_transform(
        resolve_result: PluginResolveHookResult,
        load_module_type: ModuleType,
        transform_result: PluginDriverTransformHookResult,
        module: &mut Module,
        context: &Arc<CompilationContext>,
    ) -> Result<Vec<PluginAnalyzeDepsHookResultEntry>> {
        // MARK: PARSE
        let parse_param = PluginParseHookParam {
            module_id: module.id.clone(),
            resolved_path: resolve_result.resolved_path.clone(),
            query: resolve_result.query.clone(),
            module_type: transform_result.module_type.unwrap_or(load_module_type),
            content: Arc::new(transform_result.content),
        };

        let mut module_meta: ModuleMetaData =
            call_and_catch_error!(parse, Arc::new(parse_param.clone()), context);

        // MARK: PROCESS MODULE
        if let Err(e) = context
            .plugin_driver
            .process_module(
                &mut PluginProcessModuleHookParam {
                    module_id: &parse_param.module_id,
                    module_type: &parse_param.module_type,
                    content: module.content.clone(),
                    meta: &mut module_meta,
                },
                context,
            )
            .await
        {
            return Err(CompilationError::ProcessModuleError {
                resolved_path: resolve_result.resolved_path,
                source: Some(Box::new(e)),
            });
        }

        module.size = parse_param.content.as_bytes().len();
        module.module_type = parse_param.module_type;
        module.side_effects = resolve_result.side_effects;
        module.external = false;
        module.source_map_chain = transform_result.source_map_chain;
        module.meta = Box::new(module_meta);

        let _resolved_path = module.id.resolved_path(&context.config.root);
        // let package_info =
        //     load_package_json(PathBuf::from(resolved_path), Default::default()).unwrap_or_default();
        // module.package_name = package_info.name.unwrap_or("default".to_string());
        // module.package_version = package_info.version.unwrap_or("0.0.0".to_string());

        Ok(vec![])
    }
}

fn handle_cached_dependency(
    cached_dependency: &ModuleId,
    module_graph: &mut ModuleGraph,
    context: &Arc<CompilationContext>,
) -> Result<Option<ResolveModuleResult>> {
    let module_cache_manager = &context.cache_manager.module_cache;

    if module_cache_manager.has_cache(cached_dependency) {
        // todo: to finish plugin driver and handle persistent cache
        let _cached_module = module_cache_manager.get_cache_ref(cached_dependency);
        let should_invalidate_cached_module = true;

        if should_invalidate_cached_module {
            module_cache_manager.invalidate_cache(cached_dependency);
        } else {
            Compiler::insert_dummy_module(cached_dependency, module_graph);
            return Ok(Some(ResolveModuleResult::Cached(cached_dependency.clone())));
        }
    }

    Ok(None)
}

// This function spawns a task for a single dependency
fn spawn_dependency_task(
    params: BuildModuleGraphParams,
) -> JoinHandle<core::result::Result<(), CompilationError>> {
    tokio::spawn(async move {
        Compiler::build_module_graph(params).await;
        Ok(())
    })
}

// MARK: HANDLE DEPENDENCIES
async fn handle_dependencies(params: HandleDependenciesParams) {
    let HandleDependenciesParams {
        module,
        resolve_param,
        order,
        deps,
        err_sender,
        context,
    } = params;

    let module_id = module.id.clone();
    let immutable = module.immutable;

    // Add module to the graph
    Compiler::add_module(module, &resolve_param.kind, &context).await;
    // Add edge to the graph
    Compiler::add_edge(&resolve_param, module_id.clone(), order, &context).await;

    // Prepare and spawn tasks for each dependency
    let futures: Vec<JoinHandle<core::result::Result<(), CompilationError>>> = deps
        .into_iter()
        .enumerate()
        .map(|(dep_order, (dep, cached_dependency))| {
            let params = BuildModuleGraphParams {
                resolve_param: PluginResolveHookParam {
                    source: dep.source,
                    importer: Some(module_id.clone()),
                    kind: dep.kind,
                },
                context: Arc::clone(&context),
                err_sender: err_sender.clone(),
                order: dep_order,
                cached_dependency: if immutable { cached_dependency } else { None },
            };
            spawn_dependency_task(params)
        })
        .collect();

    // Wait for all tasks to complete and handle errors
    join_all(futures)
        .await
        .into_iter()
        .filter_map(|result| match result {
            Ok(Ok(())) => None, // Task completed successfully
            Ok(Err(compilation_error)) => Some(compilation_error),
            Err(join_error) => Some(CompilationError::from(join_error)),
        })
        .for_each(|error| {
            let err_sender = err_sender.clone();
            tokio::spawn(async move {
                if let Err(e) = err_sender.send(error).await {
                    eprintln!("Failed to send error: {:?}", e);
                }
            });
        });
}

// MARK: RESOLVE MODULE
async fn resolve_module(
    resolve_param: &PluginResolveHookParam,
    cached_dependency: Option<ModuleId>,
    context: &Arc<CompilationContext>,
) -> Result<ResolveModuleResult> {
    let mut resolve_module_id_result = None;
    let module_id = if let Some(cached_dependency) = &cached_dependency {
        cached_dependency.clone()
    } else {
        resolve_module_id_result = Some(Compiler::resolve_module_id(resolve_param, context).await?);
        resolve_module_id_result.as_ref().unwrap().module_id.clone()
    };

    let mut module_graph: tokio::sync::RwLockWriteGuard<ModuleGraph> =
        context.module_graph.write().await;

    if module_graph.has_module(&module_id) {
        return Ok(ResolveModuleResult::Built(module_id));
    }

    if let Some(cached_dependency) = cached_dependency {
        if let Some(result) =
            handle_cached_dependency(&cached_dependency, &mut module_graph, context)?
        {
            return Ok(result);
        }
    }

    let resolve_module_id_result = if let Some(result) = resolve_module_id_result {
        result
    } else {
        Compiler::resolve_module_id(resolve_param, context).await?
    };

    Compiler::insert_dummy_module(&resolve_module_id_result.module_id, &mut module_graph);

    // todo: handle immutable modules
    // let module_id_str = resolve_module_id_result.module_id.to_string();
    // let immutable = !module_id_str.ends_with(DYNAMIC_VIRTUAL_SUFFIX) &&
    // context.config.partial_bundling.immutable_modules.iter().any(|im| im.is_match(&module_id_str)),

    let module = Compiler::create_module(
        resolve_module_id_result.module_id.clone(),
        resolve_module_id_result.resolve_result.external,
        false,
    );

    Ok(ResolveModuleResult::Success(Box::new(ResolvedModuleInfo {
        module,
        resolve_module_id_result,
    })))
}
