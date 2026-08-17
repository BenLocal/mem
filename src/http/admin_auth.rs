use std::future::Future;

use axum::{
    extract::FromRequestParts,
    http::{header::AUTHORIZATION, request::Parts},
};

use crate::{app::AppState, error::AppError};

/// Request-parts extractor for privileged HTTP routes.
///
/// Handlers place this before `Json<_>` so an unauthorized request is rejected
/// before an attacker-controlled body is parsed or allocated.
pub struct AdminAuthorized;

impl FromRequestParts<AppState> for AdminAuthorized {
    type Rejection = AppError;

    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let authorization = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        let authorized = state.config.authorizes_admin_bearer(authorization);
        async move {
            if authorized {
                Ok(Self)
            } else {
                Err(AppError::unauthorized_admin())
            }
        }
    }
}

macro_rules! role_authorized {
    ($name:ident, $role_authorizer:ident) => {
        pub struct $name {
            tenant_scope: Option<String>,
        }

        impl $name {
            pub fn tenant_scope(&self) -> Option<&str> {
                self.tenant_scope.as_deref()
            }

            pub fn require_tenant(&self, tenant: &str) -> Result<(), AppError> {
                if self
                    .tenant_scope
                    .as_deref()
                    .is_none_or(|allowed| allowed == tenant)
                {
                    Ok(())
                } else {
                    Err(AppError::unauthorized_admin())
                }
            }

            pub fn is_superuser(&self) -> bool {
                self.tenant_scope.is_none()
            }
        }

        impl FromRequestParts<AppState> for $name {
            type Rejection = AppError;

            fn from_request_parts(
                parts: &mut Parts,
                state: &AppState,
            ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
                let authorization = parts
                    .headers
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok());
                let superuser = state.config.authorizes_admin_bearer(authorization);
                let role_authorized = state.config.$role_authorizer(authorization);
                let role_tenant = state.config.skill_auth_tenant().to_string();
                async move {
                    if superuser {
                        Ok(Self { tenant_scope: None })
                    } else if role_authorized {
                        Ok(Self {
                            tenant_scope: Some(role_tenant),
                        })
                    } else {
                        Err(AppError::unauthorized_admin())
                    }
                }
            }
        }
    };
}

role_authorized!(CompilerAuthorized, authorizes_skill_compiler_role_bearer);
role_authorized!(ReviewerAuthorized, authorizes_skill_reviewer_role_bearer);
role_authorized!(RuntimeAuthorized, authorizes_skill_runtime_role_bearer);
