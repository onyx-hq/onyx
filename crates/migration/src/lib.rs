pub use sea_orm_migration::prelude::*;

mod m20220101_000001_create_table;
mod m20241111_110133_add_agent_to_conversation;
mod m20241112_035850_add_message;
mod m20250307_090813_add_threads;
mod m20250318_230139_add_thread_references;
mod m20250501_215840_add_tasks;
mod m20250519_011103_add_workflow_to_threads;
mod m20250522_011451_drop_messages_and_conversations;
mod m20250523_123859_add_users_table;
mod m20250523_123900_add_user_id_to_threads;
mod m20250527_005652_create_table_messages;
mod m20250609_000001_create_api_keys_table;
#[allow(non_snake_case)]
mod m20250609_015141_Add_artifacts;
mod m20250611_015638_add_tokens_to_messages;
mod m20250613_090405_add_auth_fields_to_users;
mod m20250618_102934_create_github_config_table;
mod m20250624_100000_add_role_to_users;
mod m20250625_000001_add_status_to_users;
mod m20250625_151048_add_is_processing_to_thread;
mod m20250626_000001_create_secrets_table;
mod m20250708_021201_create_logs_table;
mod m20250727_150336_add_run_model;
mod m20250811_084647_add_organizations_table;
mod m20250811_084822_add_organization_users_table;
mod m20250811_085101_projects_table;
mod m20250811_090444_branches_table;
mod m20250813_071440_add_project_id_to_threads_table;
mod m20250813_071500_add_project_id_to_secrets_table;
mod m20250813_071600_add_project_id_to_runs_table;
mod m20250819_020551_add_project_id_to_api_keys_table;
mod m20250819_084109_fix_root_replay_ref_type;
mod m20250902_073902_add_active_branch_id_to_projects_table;
mod m20250902_080016_add_branch_id_to_runs_table;
mod m20250902_080217_add_project_branch_id_to_run_index_runs_table;
mod m20250923_064746_change_organization_to_workspace;
mod m20250924_015621_create_project_repos_and_update_projects;
mod m20250929_081920_create_git_namespaces_table;
mod m20251009_020233_add_run_variables_and_output;
mod m20251104_015138_add_blocks_to_message;
mod m20251204_000001_create_a2a_tasks_table;
mod m20251204_000002_create_a2a_messages_table;
mod m20251204_000003_create_a2a_task_status_table;
mod m20251204_000004_create_a2a_artifacts_table;
mod m20251219_000001_add_user_id_to_runs;
mod m20260108_000001_drop_fk_runs_project_id;
mod m20260109_000001_add_sandbox_fields_to_threads;
mod m20260302_000001_add_magic_link_to_users;
mod m20260302_000002_drop_email_verification_token;
mod m20260302_000003_drop_password_hash;
mod m20260304_000001_create_testing_tables;
mod m20260312_000001_create_run_sequences_table;
mod m20260317_000001_create_agentic_tables;
mod m20260317_000002_rename_legacy_agentic_tables;
mod m20260318_000001_add_thread_id_to_agentic_runs;
mod m20260324_000001_add_updated_by_to_secrets;
mod m20260328_000001_drop_workspace_tables;
mod m20260329_000001_add_path_to_projects;
mod m20260330_000001_add_created_by_to_projects;
mod m20260401_000001_add_owner_role;
mod m20260401_000001_add_spec_hint_to_agentic_runs;
mod m20260402_000001_add_thinking_mode_to_agentic_runs;
mod m20260402_000001_rename_projects_to_workspaces;
mod m20260408_000001_create_organizations;
mod m20260408_000002_create_org_members;
mod m20260408_000003_create_org_invitations;
mod m20260408_000004_create_workspace_members;
mod m20260408_000005_add_org_id_to_tables;
mod m20260409_000001_add_github_token_to_users;
mod m20260414_000001_drop_github_token_from_users;
mod m20260415_000001_create_github_accounts;
mod m20260415_000002_null_installation_namespace_tokens;
mod m20260416_000001_add_status_and_error_to_workspaces;
mod m20260416_000001_create_observability_tables;
mod m20260416_000002_swap_workspace_repo_for_git_namespace;
mod m20260416_000003_drop_branches_and_workspace_repos;
mod m20260424_000001_create_org_billing;
mod m20260424_000002_create_stripe_webhook_events;
mod m20260430_000001_create_feature_flags;
mod m20260525_000001_create_metric_anomalies;
mod m20260526_000001_metric_anomalies_explain_cache;
mod m20260528_000001_create_customer_apps_schema;
mod m20260528_000002_apps_add_repo_path;
mod m20260528_000003_create_app_builds;
mod m20260528_000004_app_builds_add_published_by;
mod m20260528_000005_apps_add_last_promoted;
mod m20260604_000001_metric_anomalies_filters;
mod m20260612_000001_create_app_functions;
// Legacy single-tenant Slack tables. The original CREATE migrations were
// deleted when the universal multi-tenant Slack bot replaced them, but
// dev/prod databases that had already applied them required the files
// to remain on disk for SeaORM's startup integrity check (every row in
// `seaql_migrations` must have a corresponding file). Restored verbatim
// from commit ca9def9d7 — the drop migration below removes the tables.
mod m20251114_000002_create_slack_channel_bindings_table;
mod m20251114_000003_create_slack_user_identities_table;
mod m20251114_000004_create_slack_conversation_contexts_table;
mod m20260421_000001_drop_legacy_slack_tables;
mod m20260421_000002_create_org_secrets;
mod m20260421_000003_create_slack_installations;
mod m20260421_000004_create_slack_user_links;
mod m20260421_000005_create_slack_user_preferences;
mod m20260421_000006_create_slack_threads;
mod m20260421_000007_create_slack_oauth_states;
mod m20260422_000001_create_slack_seen_events;
mod m20260424_000001_create_slack_channel_defaults;
mod m20260427_000001_slack_oauth_state_add_channel;
mod m20260529_000001_add_vlm_budget_to_workspaces;
mod m20260601_000001_api_keys_add_app_id;
mod m20260602_000001_create_quickbooks_oauth_states;
mod m20260606_000001_create_custom_app_tracking;
mod m20260606_000002_create_compile_boundary;
mod m20260612_000001_add_logo_to_organizations;
mod m20260622_000001_create_org_subdomains;
mod m20260622_000001_create_workspace_health_state;
mod m20260623_000001_rename_procedures_to_automations;
mod m20260624_000001_create_reconcile_configs;
mod m20260624_000001_create_world_model_configs;
mod m20260624_000002_health_state_add_payload;
mod m20260630_000001_create_backfill_checkpoints;
mod m20260701_000001_add_workspace_id_to_backfill_checkpoints;
mod m20260702_000001_create_app_publish_tokens;
mod m20260702_000001_create_backfill_ranges;
mod m20260707_000001_app_builds_add_git_source;
mod m20260710_000001_health_state_add_last_smoke_at;
mod m20260712_000001_add_message_thread_indexes;
mod m20260713_000001_partner_platform;
mod m20260715_000001_app_publishing_auth;
mod m20260715_000002_oidc_used_jti;
mod m20260715_000003_publish_token_nullable_creator;
mod m20260716_000001_app_build_validation_status;
mod m20260722_000001_app_visibility_and_members;
mod m20260723_000001_metric_anomalies_seasonal_period;
mod m20260728_000001_health_state_add_alert_tracking;
mod m20260729_000001_metric_monitor_coverage;
mod m20260729_000002_metric_anomalies_event_id;
mod m20260729_000003_metric_anomalies_granularity_key;
mod m20260730_000001_org_teams_and_app_grants;
mod m20260804_000001_metric_anomalies_cohort;
mod m20260805_000001_airway_source_config;
mod m20260806_000001_platform_grants;
mod m20260806_000002_create_app_storage_usage;
mod m20260807_000001_airway_deployment_config;
mod m20260808_000001_airway_deployment_cursor_lag_floor;
mod m20260812_000001_create_simulation_definitions;
mod m20260812_000001_metric_anomalies_event_count_index;
mod m20260812_000002_create_simulation_runs;
mod m20260817_000001_create_schema_migration_definitions;
mod m20260819_000001_simulation_run_replicate;
mod m20260821_000001_view_event_roles;
mod m20260829_000001_frontline_identity;
mod m20260830_000001_oauth_states_provider;
mod m20260831_000001_world_model_events;
mod m20260901_000001_chat;
mod m20260901_000002_assignment_graph;
mod m20260901_000003_notifications;
mod m20260902_000001_simulation_run_queued_at;
mod m20260903_000001_function_result_status;
mod m20260905_000001_create_custom_app_migrations;
mod m20260906_000001_org_kiosk_devices;
mod m20260907_000001_operating_graph;
mod m20260908_000001_audit_events_append_only;
mod m20260908_000002_drop_checkpoints;
mod m20260911_000001_kiosk_idle_timeout;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20220101_000001_create_table::Migration),
            Box::new(m20241111_110133_add_agent_to_conversation::Migration),
            Box::new(m20241112_035850_add_message::Migration),
            Box::new(m20250307_090813_add_threads::Migration),
            Box::new(m20250318_230139_add_thread_references::Migration),
            Box::new(m20250501_215840_add_tasks::Migration),
            Box::new(m20250519_011103_add_workflow_to_threads::Migration),
            Box::new(m20250522_011451_drop_messages_and_conversations::Migration),
            Box::new(m20250523_123859_add_users_table::Migration),
            Box::new(m20250523_123900_add_user_id_to_threads::Migration),
            Box::new(m20250527_005652_create_table_messages::Migration),
            Box::new(m20250609_000001_create_api_keys_table::Migration),
            Box::new(m20250609_015141_Add_artifacts::Migration),
            Box::new(m20250611_015638_add_tokens_to_messages::Migration),
            Box::new(m20250613_090405_add_auth_fields_to_users::Migration),
            Box::new(m20250618_102934_create_github_config_table::Migration),
            Box::new(m20250624_100000_add_role_to_users::Migration),
            Box::new(m20250625_000001_add_status_to_users::Migration),
            Box::new(m20250625_151048_add_is_processing_to_thread::Migration),
            Box::new(m20250626_000001_create_secrets_table::Migration),
            Box::new(m20250708_021201_create_logs_table::Migration),
            Box::new(m20250727_150336_add_run_model::Migration),
            Box::new(m20250811_084647_add_organizations_table::Migration),
            Box::new(m20250811_084822_add_organization_users_table::Migration),
            Box::new(m20250811_085101_projects_table::Migration),
            Box::new(m20250811_090444_branches_table::Migration),
            Box::new(m20250813_071440_add_project_id_to_threads_table::Migration),
            Box::new(m20250813_071500_add_project_id_to_secrets_table::Migration),
            Box::new(m20250813_071600_add_project_id_to_runs_table::Migration),
            Box::new(m20250819_020551_add_project_id_to_api_keys_table::Migration),
            Box::new(m20250819_084109_fix_root_replay_ref_type::Migration),
            Box::new(m20250902_073902_add_active_branch_id_to_projects_table::Migration),
            Box::new(m20250902_080016_add_branch_id_to_runs_table::Migration),
            Box::new(m20250902_080217_add_project_branch_id_to_run_index_runs_table::Migration),
            Box::new(m20250923_064746_change_organization_to_workspace::Migration),
            Box::new(m20250924_015621_create_project_repos_and_update_projects::Migration),
            Box::new(m20250929_081920_create_git_namespaces_table::Migration),
            Box::new(m20251009_020233_add_run_variables_and_output::Migration),
            Box::new(m20251104_015138_add_blocks_to_message::Migration),
            Box::new(m20251204_000001_create_a2a_tasks_table::Migration),
            Box::new(m20251204_000002_create_a2a_messages_table::Migration),
            Box::new(m20251204_000003_create_a2a_task_status_table::Migration),
            Box::new(m20251204_000004_create_a2a_artifacts_table::Migration),
            Box::new(m20251219_000001_add_user_id_to_runs::Migration),
            Box::new(m20260108_000001_drop_fk_runs_project_id::Migration),
            Box::new(m20260109_000001_add_sandbox_fields_to_threads::Migration),
            Box::new(m20260302_000001_add_magic_link_to_users::Migration),
            Box::new(m20260302_000002_drop_email_verification_token::Migration),
            Box::new(m20260302_000003_drop_password_hash::Migration),
            Box::new(m20260304_000001_create_testing_tables::Migration),
            Box::new(m20260312_000001_create_run_sequences_table::Migration),
            Box::new(m20260317_000001_create_agentic_tables::Migration),
            Box::new(m20260317_000002_rename_legacy_agentic_tables::Migration),
            Box::new(m20260318_000001_add_thread_id_to_agentic_runs::Migration),
            Box::new(m20260324_000001_add_updated_by_to_secrets::Migration),
            Box::new(m20260328_000001_drop_workspace_tables::Migration),
            Box::new(m20260329_000001_add_path_to_projects::Migration),
            Box::new(m20260330_000001_add_created_by_to_projects::Migration),
            Box::new(m20260401_000001_add_owner_role::Migration),
            Box::new(m20260401_000001_add_spec_hint_to_agentic_runs::Migration),
            Box::new(m20260402_000001_add_thinking_mode_to_agentic_runs::Migration),
            Box::new(m20260402_000001_rename_projects_to_workspaces::Migration),
            Box::new(m20260408_000001_create_organizations::Migration),
            Box::new(m20260408_000002_create_org_members::Migration),
            Box::new(m20260408_000003_create_org_invitations::Migration),
            Box::new(m20260408_000004_create_workspace_members::Migration),
            Box::new(m20260408_000005_add_org_id_to_tables::Migration),
            Box::new(m20260409_000001_add_github_token_to_users::Migration),
            Box::new(m20260414_000001_drop_github_token_from_users::Migration),
            Box::new(m20260415_000001_create_github_accounts::Migration),
            Box::new(m20260415_000002_null_installation_namespace_tokens::Migration),
            Box::new(m20260416_000001_add_status_and_error_to_workspaces::Migration),
            Box::new(m20260416_000002_swap_workspace_repo_for_git_namespace::Migration),
            Box::new(m20260416_000003_drop_branches_and_workspace_repos::Migration),
            Box::new(m20260416_000001_create_observability_tables::Migration),
            Box::new(m20260424_000001_create_org_billing::Migration),
            Box::new(m20260424_000002_create_stripe_webhook_events::Migration),
            Box::new(m20260430_000001_create_feature_flags::Migration),
            Box::new(m20260528_000001_create_customer_apps_schema::Migration),
            Box::new(m20260528_000002_apps_add_repo_path::Migration),
            Box::new(m20260528_000003_create_app_builds::Migration),
            Box::new(m20260528_000004_app_builds_add_published_by::Migration),
            Box::new(m20260528_000005_apps_add_last_promoted::Migration),
            Box::new(m20260612_000001_create_app_functions::Migration),
            // Legacy single-tenant Slack tables — see module-level comment above.
            Box::new(m20251114_000002_create_slack_channel_bindings_table::Migration),
            Box::new(m20251114_000003_create_slack_user_identities_table::Migration),
            Box::new(m20251114_000004_create_slack_conversation_contexts_table::Migration),
            Box::new(m20260421_000001_drop_legacy_slack_tables::Migration),
            Box::new(m20260421_000002_create_org_secrets::Migration),
            Box::new(m20260421_000003_create_slack_installations::Migration),
            Box::new(m20260421_000004_create_slack_user_links::Migration),
            Box::new(m20260421_000005_create_slack_user_preferences::Migration),
            Box::new(m20260421_000006_create_slack_threads::Migration),
            Box::new(m20260421_000007_create_slack_oauth_states::Migration),
            Box::new(m20260422_000001_create_slack_seen_events::Migration),
            Box::new(m20260424_000001_create_slack_channel_defaults::Migration),
            Box::new(m20260427_000001_slack_oauth_state_add_channel::Migration),
            Box::new(m20260525_000001_create_metric_anomalies::Migration),
            Box::new(m20260526_000001_metric_anomalies_explain_cache::Migration),
            Box::new(m20260529_000001_add_vlm_budget_to_workspaces::Migration),
            Box::new(m20260601_000001_api_keys_add_app_id::Migration),
            Box::new(m20260602_000001_create_quickbooks_oauth_states::Migration),
            Box::new(m20260604_000001_metric_anomalies_filters::Migration),
            Box::new(m20260606_000001_create_custom_app_tracking::Migration),
            Box::new(m20260606_000002_create_compile_boundary::Migration),
            Box::new(m20260612_000001_add_logo_to_organizations::Migration),
            Box::new(m20260622_000001_create_org_subdomains::Migration),
            Box::new(m20260622_000001_create_workspace_health_state::Migration),
            Box::new(m20260623_000001_rename_procedures_to_automations::Migration),
            Box::new(m20260624_000001_create_reconcile_configs::Migration),
            Box::new(m20260624_000001_create_world_model_configs::Migration),
            Box::new(m20260624_000002_health_state_add_payload::Migration),
            Box::new(m20260630_000001_create_backfill_checkpoints::Migration),
            Box::new(m20260701_000001_add_workspace_id_to_backfill_checkpoints::Migration),
            Box::new(m20260702_000001_create_app_publish_tokens::Migration),
            Box::new(m20260702_000001_create_backfill_ranges::Migration),
            Box::new(m20260707_000001_app_builds_add_git_source::Migration),
            Box::new(m20260710_000001_health_state_add_last_smoke_at::Migration),
            Box::new(m20260712_000001_add_message_thread_indexes::Migration),
            Box::new(m20260713_000001_partner_platform::Migration),
            Box::new(m20260715_000001_app_publishing_auth::Migration),
            Box::new(m20260715_000002_oidc_used_jti::Migration),
            Box::new(m20260715_000003_publish_token_nullable_creator::Migration),
            Box::new(m20260716_000001_app_build_validation_status::Migration),
            Box::new(m20260722_000001_app_visibility_and_members::Migration),
            Box::new(m20260723_000001_metric_anomalies_seasonal_period::Migration),
            Box::new(m20260728_000001_health_state_add_alert_tracking::Migration),
            Box::new(m20260729_000001_metric_monitor_coverage::Migration),
            Box::new(m20260729_000002_metric_anomalies_event_id::Migration),
            Box::new(m20260729_000003_metric_anomalies_granularity_key::Migration),
            Box::new(m20260730_000001_org_teams_and_app_grants::Migration),
            Box::new(m20260804_000001_metric_anomalies_cohort::Migration),
            Box::new(m20260805_000001_airway_source_config::Migration),
            Box::new(m20260806_000001_platform_grants::Migration),
            Box::new(m20260806_000002_create_app_storage_usage::Migration),
            Box::new(m20260807_000001_airway_deployment_config::Migration),
            Box::new(m20260808_000001_airway_deployment_cursor_lag_floor::Migration),
            Box::new(m20260812_000001_create_simulation_definitions::Migration),
            Box::new(m20260812_000001_metric_anomalies_event_count_index::Migration),
            Box::new(m20260812_000002_create_simulation_runs::Migration),
            Box::new(m20260817_000001_create_schema_migration_definitions::Migration),
            Box::new(m20260819_000001_simulation_run_replicate::Migration),
            Box::new(m20260821_000001_view_event_roles::Migration),
            Box::new(m20260829_000001_frontline_identity::Migration),
            Box::new(m20260830_000001_oauth_states_provider::Migration),
            Box::new(m20260831_000001_world_model_events::Migration),
            Box::new(m20260901_000001_chat::Migration),
            Box::new(m20260901_000002_assignment_graph::Migration),
            Box::new(m20260901_000003_notifications::Migration),
            Box::new(m20260902_000001_simulation_run_queued_at::Migration),
            Box::new(m20260903_000001_function_result_status::Migration),
            Box::new(m20260905_000001_create_custom_app_migrations::Migration),
            Box::new(m20260906_000001_org_kiosk_devices::Migration),
            Box::new(m20260907_000001_operating_graph::Migration),
            Box::new(m20260908_000001_audit_events_append_only::Migration),
            Box::new(m20260908_000002_drop_checkpoints::Migration),
            Box::new(m20260911_000001_kiosk_idle_timeout::Migration),
        ]
    }
}
