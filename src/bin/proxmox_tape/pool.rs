use anyhow::{Error};
use serde_json::Value;

use proxmox::{
    api::{
        api,
        cli::*,
        RpcEnvironment,
        ApiHandler,
    },
};

use proxmox_backup::{
    api2::{
        self,
        types::{
            MEDIA_POOL_NAME_SCHEMA,
        },
    },
    config::{
        drive::{
            complete_drive_name,
         },
        media_pool::{
            complete_pool_name,
       },
    },
};

pub fn pool_commands() -> CommandLineInterface {

    let cmd_def = CliCommandMap::new()
        .insert("list", CliCommand::new(&API_METHOD_LIST_POOLS))
        .insert("config",
                CliCommand::new(&API_METHOD_GET_CONFIG)
                .arg_param(&["name"])
                .completion_cb("name", complete_pool_name)
        )
        .insert(
            "remove",
            CliCommand::new(&api2::config::media_pool::API_METHOD_DELETE_POOL)
                .arg_param(&["name"])
                .completion_cb("name", complete_pool_name)
        )
        .insert(
            "create",
            CliCommand::new(&api2::config::media_pool::API_METHOD_CREATE_POOL)
                .arg_param(&["name"])
                .completion_cb("name", complete_pool_name)
                .completion_cb("drive", complete_drive_name)
        )
        .insert(
            "update",
            CliCommand::new(&api2::config::media_pool::API_METHOD_UPDATE_POOL)
                .arg_param(&["name"])
                .completion_cb("name", complete_pool_name)
                .completion_cb("drive", complete_drive_name)
        )
        ;

    cmd_def.into()
}

#[api(
    input: {
        properties: {
            "output-format": {
                schema: OUTPUT_FORMAT,
                optional: true,
            },
        },
    },
)]
/// List media pool
fn list_pools(
    param: Value,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<(), Error> {

    let output_format = get_output_format(&param);
    let info = &api2::config::media_pool::API_METHOD_LIST_POOLS;
    let mut data = match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    let options = default_table_format_options()
        .column(ColumnConfig::new("name"))
        .column(ColumnConfig::new("drive"))
        .column(ColumnConfig::new("allocation"))
        .column(ColumnConfig::new("retention"))
        .column(ColumnConfig::new("template"))
        ;

    format_and_print_result_full(&mut data, info.returns, &output_format, &options);

    Ok(())
}

#[api(
    input: {
        properties: {
            "output-format": {
                schema: OUTPUT_FORMAT,
                optional: true,
            },
            name: {
                schema: MEDIA_POOL_NAME_SCHEMA,
            },
        },
    },
)]
/// Get media pool configuration
fn get_config(
    param: Value,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<(), Error> {

    let output_format = get_output_format(&param);
    let info = &api2::config::media_pool::API_METHOD_GET_CONFIG;
    let mut data = match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    let options = default_table_format_options()
        .column(ColumnConfig::new("name"))
        .column(ColumnConfig::new("drive"))
        .column(ColumnConfig::new("allocation"))
        .column(ColumnConfig::new("retention"))
        .column(ColumnConfig::new("template"))
        ;

    format_and_print_result_full(&mut data, info.returns, &output_format, &options);

    Ok(())
}