use crate::validate::validate_jwt;
use crate::AuthError;
use crate::AuthResult;
use crate::TokenSigner;

use std::marker::PhantomData;

use actix_web::cookie::Cookie;
use actix_web::dev::ServiceRequest;
use actix_web::FromRequest;
use actix_web::Handler;
use actix_web::HttpMessage;
use derive_builder::Builder;
use jwt_compact::TimeOptions;
use jwt_compact::Token;
use jwt_compact::UntrustedToken;
use jwt_compact::ValidationError::Expired as TokenExpired;
use serde::de::DeserializeOwned;
use serde::Serialize;

macro_rules! pull_from_token_signer {
    ($self:ident ,$field_name:ident) => {
        match $self.token_signer {
            Some(Some(ref value)) => value.$field_name.clone(),
            _ => {
                return ::derive_builder::export::core::result::Result::Err(
                    ::derive_builder::export::core::convert::Into::into(
                        ::derive_builder::UninitializedFieldError::from(stringify!($field_name)),
                    ),
                );
            }
        }
    };

    ($self:ident, $field_name:ident, $alternative:expr) => {
        match $self.token_signer {
            Some(Some(ref value)) => value.$field_name.clone(),
            _ => $alternative,
        }
    };
}

/*
    Struct used to signal to the middleware that a cookie needs to be updated
    after the wrapped service has returned a response.
*/
#[doc(hidden)]
#[derive(Debug)]
pub struct TokenUpdate {
    pub(crate) auth_cookie: Option<Cookie<'static>>,
    pub(crate) refresh_cookie: Option<Cookie<'static>>,
}

/**
    Handles the authorization of requests for the middleware as well as refreshing the `access`/`refresh` token.

    Please referee to the [`AuthorityBuilder`] for a detailed description of options available on this struct.
*/
#[derive(Builder, Clone)]
pub struct Authority<Claims, Algorithm, RefreshAuthorizer, Args>
where
    Algorithm: jwt_compact::Algorithm,
    Algorithm::SigningKey: Clone,
    Algorithm::VerifyingKey: Clone,
{
    /**
        The `refresh_authorizer` is called every time,
        when a client with an expired access token but a valid refresh token
        tries to fetch a resource protected by the jwt middleware.

        By returning the `Ok` variant your grand the client permission to get a new access token.
        In contrast, by returning the `Err` variant you deny the request.
        The [`actix_web::Error`](actix_web::Error) returned in this case
        will be passed along as a wrapped internal [`AuthError`] back to the client
        (There are options to remap this [actix-error-mapper]).

        Since `refresh_authorizer` has to implement the [`Handler`](actix_web::dev::Handler) trait,
        you are able to access your regular application an request state from within
        the function. This allows you to perform Database Check etc...
    */
    refresh_authorizer: RefreshAuthorizer,
    /**
       Not Passing a [`TokenSigner`] struct will make your middleware unable to refresh the access token automatically.

       You will have to provide a algorithm manually in this case because the Authority can not pull it from the `token_signer` field.

       Please referee to the structs own documentation for more details.
    */
    #[builder(default = "None")]
    token_signer: Option<TokenSigner<Claims, Algorithm>>,
    /**
        Depending on wether a [`TokenSigner`] is set, setting this field will have no affect.

        Defaults to the value of the `access_token_name` field set on the `token_signer`, if the `token_signer` is not set,
        this defaults to `"access_token"`.
    */
    #[builder(default = "pull_from_token_signer!(self, access_token_name, \"access_token\")")]
    pub(crate) access_token_name: &'static str,
    /**
        Self explanatory, if set to false the clients access token will not be automatically refreshed.

        Defaults to `true`
    */
    #[builder(default = "true")]
    renew_access_token_automatically: bool,
    /**
        Depending on wether a [`TokenSigner`] is set, setting this field will have no affect.

        Defaults to the value of the `refresh_token_name` field set on the `token_signer`,
        if the `token_signer` is not set, this defaults to `"refresh_token"`.
    */
    #[builder(default = "pull_from_token_signer!(self, refresh_token_name, \"refresh_token\")")]
    pub(crate) refresh_token_name: &'static str,
    /**
        If set to true the clients refresh token will automatically refreshed,
        this allows clients to basically stay authenticated over a infinite amount of time, so i don't recommend it.

        Defaults to `false`
    */
    #[builder(default = "false")]
    renew_refresh_token_automatically: bool,
    /**
        Key used to verify integrity of access and refresh token.
    */
    verifying_key: Algorithm::VerifyingKey,
    /**
        The Cryptographic signing algorithm used in the process of creation of access and refresh tokens.

        Please referee to the [`Supported algorithms`](https://docs.rs/jwt-compact/latest/jwt_compact/#supported-algorithms) section of the `jwt-compact` crate for a comprehensive list of the supported algorithms.

        Defaults to the value of the `algorithm` field set on the `token_signer`, if the `token_signer` is not set,
        this field needs to be set.
    */
    #[builder(default = "pull_from_token_signer!(self, algorithm)")]
    algorithm: Algorithm,
    /**
        Used in the creating of the `token`, the current timestamp is taken from this, but please referee to the Structs documentation.

        Defaults to the value of the `time_options` field set on the `token_signer`, if the `token_signer` is not set,
        this field needs to be set.
    */
    #[builder(default = "pull_from_token_signer!(self, time_options)")]
    time_options: TimeOptions,
    #[doc(hidden)]
    #[builder(setter(skip), default = "PhantomData")]
    _claims: PhantomData<Claims>,
    #[doc(hidden)]
    #[builder(setter(skip), default = "PhantomData")]
    _args: PhantomData<Args>,
}

impl<Claims, Algorithm, RefreshAuthorizer, Args>
    Authority<Claims, Algorithm, RefreshAuthorizer, Args>
where
    Claims: Serialize + DeserializeOwned + Clone + 'static,
    Algorithm: jwt_compact::Algorithm + Clone,
    Algorithm::SigningKey: Clone,
    Algorithm::VerifyingKey: Clone,
    RefreshAuthorizer: Handler<Args, Output = Result<(), actix_web::Error>> + Clone,
    Args: FromRequest + Clone,
{
    /**
        Returns a new [`AuthorityBuilder`]
    */
    pub fn new() -> AuthorityBuilder<Claims, Algorithm, RefreshAuthorizer, Args> {
        AuthorityBuilder::default()
    }

    /**
        Returns a Clone of the `token_signer` field on the Authority.
    */
    pub fn token_signer(&self) -> Option<TokenSigner<Claims, Algorithm>> {
        self.token_signer.clone()
    }

    /**
        Use by the [`crate::AuthenticationMiddleware`]
        in oder to verify an incoming request and ether hand it of to protected services
        or deny the request by return a wrapped [`AuthError`].
    */
    pub async fn verify_service_request(
        &self,
        req: &mut ServiceRequest,
    ) -> AuthResult<Option<TokenUpdate>> {
        match self.validate_cookie(&req, self.access_token_name) {
            Ok(access_token) => {
                req.extensions_mut()
                    .insert(access_token.claims().custom.clone());
                Ok(None)
            }
            Err(AuthError::TokenValidation(TokenExpired) | AuthError::NoToken)
                if self.renew_access_token_automatically =>
            {
                self.call_refresh_authorizer(req).await?;
                match (
                    self.validate_cookie(&req, self.refresh_token_name),
                    &self.token_signer,
                ) {
                    (Ok(refresh_token), Some(token_signer)) => {
                        let claims = refresh_token.claims().custom.clone();
                        req.extensions_mut().insert(claims.clone());
                        Ok(Some(TokenUpdate {
                            auth_cookie: Some(token_signer.create_access_cookie(&claims)?),
                            refresh_cookie: None,
                        }))
                    }
                    (Err(AuthError::TokenValidation(TokenExpired)), Some(token_signer))
                        if self.renew_refresh_token_automatically =>
                    {
                        let claims = UntrustedToken::new(
                            req
                                .cookie(self.refresh_token_name)
                                .expect("Cookie has to be set in oder to get to this point")
                                .value(),
                        )
                        .expect("UntrustedToken token has to be parseable fro, cookie value in order to get to here")
                        .deserialize_claims_unchecked::<Claims>()
                        .expect("Claims has to be desirializeable to get to this point")
                        .custom;
                        req.extensions_mut().insert(claims.clone());
                        Ok(Some(TokenUpdate {
                            auth_cookie: Some(token_signer.create_access_cookie(&claims)?),
                            refresh_cookie: Some(token_signer.create_refresh_cookie(&claims)?),
                        }))
                    }
                    (Ok(_), None) => Err(AuthError::NoCookieSigner),
                    (Err(err), _) => Err(err),
                }
            }
            Err(err) => Err(err),
        }
    }

    fn validate_cookie(
        &self,
        req: &ServiceRequest,
        cookie_name: &'static str,
    ) -> AuthResult<Token<Claims>> {
        match req.cookie(cookie_name) {
            Some(token_value) => validate_jwt(
                &token_value.value(),
                &self.algorithm,
                &self.verifying_key,
                &self.time_options,
            ),
            None => Err(AuthError::NoToken),
        }
    }

    async fn call_refresh_authorizer(&self, req: &mut ServiceRequest) -> AuthResult<()> {
        let (mut_req, mut payload) = req.parts_mut();
        match Args::from_request(&mut_req, &mut payload).await {
            Ok(args) => self
                .refresh_authorizer
                .call(args)
                .await
                .map_err(|err| AuthError::RefreshAuthorizerDenied(err)),
            Err(err) => Err(AuthError::Internal(err.into())),
        }
    }
}