use sea_orm_migration::prelude::*;

/// App publish tokens: long-lived bearer credentials for machine auth (primarily
/// `oxyc publish` in CI), minted by global app-admins. Only a hash of the
/// token is stored, never the plaintext. A live (non-revoked) token
/// authenticates as its owner *only on the customer-apps admin surface* —
/// scope enforcement lives in the request path, not this schema.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
            CREATE TABLE app_publish_tokens (
                id UUID PRIMARY KEY,
                name TEXT NOT NULL,
                token_hash TEXT NOT NULL UNIQUE,
                token_prefix TEXT NOT NULL,
                created_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                last_used_at TIMESTAMPTZ,
                revoked_at TIMESTAMPTZ
            );
            CREATE INDEX idx_app_publish_tokens_live
                ON app_publish_tokens(token_hash) WHERE revoked_at IS NULL;
        "#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS app_publish_tokens CASCADE")
            .await?;
        Ok(())
    }
}
