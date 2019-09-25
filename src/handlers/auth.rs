use bridges;
use email_address::EmailAddress;
use error::BrokerError;
use futures::future::{self, Future, Either};
use http::{ContextHandle, HandlerResult, ReturnParams, json_response};
use hyper::Method;
use hyper::header::ContentType;
use hyper::server::Response;
use mustache;
use serde_json::{Value, from_value};
use std::rc::Rc;
use std::time::Duration;
use store_limits::addr_limiter;
use tokio_core::reactor::Timeout;
use validation::parse_redirect_uri;
use webfinger::{self, Link, Relation};


/// Request handler to return the OpenID Discovery document.
///
/// Most of this is hard-coded for now, although the URLs are constructed by
/// using the base URL as configured in the `public_url` configuration value.
pub fn discovery(ctx_handle: &ContextHandle) -> HandlerResult {
    let ctx = ctx_handle.borrow();

    let obj = json!({
        "issuer": ctx.app.public_url,
        "authorization_endpoint": format!("{}/auth", ctx.app.public_url),
        "jwks_uri": format!("{}/keys.json", ctx.app.public_url),
        "scopes_supported": vec!["openid", "email"],
        "claims_supported": vec!["iss", "aud", "exp", "iat", "email"],
        "response_types_supported": vec!["id_token"],
        "response_modes_supported": vec!["form_post", "fragment"],
        "grant_types_supported": vec!["implicit"],
        "subject_types_supported": vec!["public"],
        "id_token_signing_alg_values_supported": vec!["RS256"],
    });
    Box::new(json_response(&obj, ctx.app.discovery_ttl))
}


/// Request handler for the JSON Web Key Set document.
///
/// Respond with the JWK key set containing all of the configured keys.
///
/// Relying Parties will need to fetch this data to be able to verify identity
/// tokens issued by this daemon instance.
pub fn key_set(ctx_handle: &ContextHandle) -> HandlerResult {
    let ctx = ctx_handle.borrow();

    let obj = json!({
        "keys": ctx.app.keys.iter()
            .map(|key| key.public_jwk())
            .collect::<Vec<_>>(),
    });
    Box::new(json_response(&obj, ctx.app.keys_ttl))
}


/// Request handler for authentication requests from the RP.
///
/// Calls the `oidc::request()` function if the provided email address's
/// domain matches one of the configured famous providers. Otherwise, calls the
/// `email::request()` function to allow authentication through the email loop.
pub fn auth(ctx_handle: &ContextHandle) -> HandlerResult {
    let mut ctx = ctx_handle.borrow_mut();
    let mut params = match ctx.method {
        Method::Get => ctx.query_params(),
        Method::Post => ctx.form_params(),
        _ => unreachable!(),
    };

    let original_params = params.clone();

    let redirect_uri = try_get_input_param!(params, "redirect_uri");
    let client_id = try_get_input_param!(params, "client_id");
    let response_mode = try_get_input_param!(params, "response_mode", "fragment".to_owned());
    let response_errors = try_get_input_param!(params, "response_errors", "true".to_owned());
    let state = try_get_input_param!(params, "state", "".to_owned());

    let redirect_uri = match parse_redirect_uri(&redirect_uri, "redirect_uri") {
        Ok(url) => url,
        Err(e) => return Box::new(future::err(BrokerError::Input(format!("{}", e)))),
    };

    if client_id != redirect_uri.origin().ascii_serialization() {
        return Box::new(future::err(BrokerError::Input(
            "the client_id must be the origin of the redirect_uri".to_owned())));
    }

    // Parse response_mode by wrapping it a JSON Value.
    // This has minimal overhead, and saves us a separate implementation.
    let response_mode = match from_value(Value::String(response_mode)) {
        Ok(response_mode) => response_mode,
        Err(_) => return Box::new(future::err(BrokerError::Input(
            "unsupported response_mode, must be fragment or form_post".to_owned()))),
    };

    let response_errors = match response_errors.parse::<bool>() {
        Ok(value) => value,
        Err(_) => return Box::new(future::err(BrokerError::Input(
            "response_errors must be true or false".to_owned()))),
    };

    // Per the OAuth2 spec, we may redirect to the RP once we have validated client_id and
    // redirect_uri. In our case, this means we make redirect_uri available to error handling.
    let redirect_uri_ = redirect_uri.clone();
    ctx.return_params = Some(ReturnParams { redirect_uri, response_mode, response_errors, state });

    if let Some(ref whitelist) = ctx.app.allowed_origins {
        if !whitelist.contains(&client_id) {
            return Box::new(future::err(BrokerError::Input(
                "the origin is not whitelisted".to_owned())));
        }
    }

    let nonce = try_get_input_param!(params, "nonce");
    if try_get_input_param!(params, "response_type") != "id_token" {
        return Box::new(future::err(BrokerError::Input(
            "unsupported response_type, only id_token is supported".to_owned())));
    }

    let login_hint = try_get_input_param!(params, "login_hint", "".to_string());
    if login_hint == "" {
        let catalog = ctx.catalog();
        let data = mustache::MapBuilder::new()
            // TODO: catalog/localization?
            .insert_str("display_origin", redirect_uri_.to_string())
            .insert_str("title", catalog.gettext("Finish logging in to"))
            .insert_str("form_action", format!("{}/auth", &ctx.app.public_url))
            .insert_str("method", ctx.method.to_string())
            .insert_str("explanation", catalog.gettext("Login with your email."))
            .insert_str("use", catalog.gettext("Please specify the email you wish to use to login with"))
            .insert_vec("params", |mut builder| {
                for param in &original_params {
                    builder = builder.push_map(|builder| {
                        let (name, value) = param;
                        builder.insert_str("name", name).insert_str("value", value)
                    });
                }
                builder
            })
            .build();

        let res = Response::new()
            .with_header(ContentType::html())
            .with_body(ctx.app.templates.login_hint.render_data(&data));
        let bf = Box::new(future::ok(res));
        return bf;
    }

    // Verify and normalize the email.
    let email_addr = match login_hint.parse::<EmailAddress>() {
        Ok(addr) => Rc::new(addr),
        Err(_) => return Box::new(future::err(BrokerError::Input(
            "login_hint is not a valid email address".to_owned()))),
    };

    // Enforce ratelimit based on the normalized email.
    match addr_limiter(&ctx.app.store, email_addr.as_str(), &ctx.app.limit_per_email) {
        Err(err) => return Box::new(future::err(err)),
        Ok(false) => return Box::new(future::err(BrokerError::RateLimited)),
        _ => {},
    }

    // Create the session with common data, but do not yet save it.
    ctx.start_session(&client_id, &login_hint, &email_addr, &nonce);

    // Discover the authentication endpoints based on the email domain.
    let f = webfinger::query(&ctx.app, &email_addr);

    // Try to authenticate with the first provider.
    // TODO: Queue discovery of links and process in order, with individual timeouts.
    let ctx_handle2 = Rc::clone(ctx_handle);
    let email_addr2 = Rc::clone(&email_addr);
    let f = f.and_then(move |links| {
        match links.first() {
            // Portier and Google providers share an implementation
            Some(link @ &Link { rel: Relation::OidcIssuer, .. })
                | Some(link @ &Link { rel: Relation::Portier, .. })
                | Some(link @ &Link { rel: Relation::Google, .. })
                => bridges::oidc::auth(&ctx_handle2, &email_addr2, link),
            _ => Box::new(future::err(BrokerError::ProviderCancelled)),
        }
    });

    // Apply a timeout to discovery.
    let ctx_handle2 = Rc::clone(ctx_handle);
    let email_addr2 = Rc::clone(&email_addr);
    let f = Timeout::new(Duration::from_secs(5), &ctx.app.handle)
        .expect("failed to create discovery timeout")
        .select2(f)
        .then(move |result| {
            match result {
                // Timeout resolved first.
                Ok(Either::A((_, f))) => {
                    // Continue the discovery future in the background.
                    ctx_handle2.borrow().app.handle.spawn(
                        f.map(|_| ()).map_err(|e| { e.log(); () }));
                    Err(BrokerError::Provider(
                        format!("discovery timed out for {}", email_addr2)))
                },
                Err(Either::A((e, _))) => {
                    panic!("error in discovery timeout: {}", e)
                },
                // Discovery resolved first.
                Ok(Either::B((v, _))) => {
                    Ok(v)
                },
                Err(Either::B((e, _))) => {
                    Err(e)
                },
            }
        });

    // Fall back to email loop authentication.
    let ctx_handle2 = Rc::clone(ctx_handle);
    let f = f.or_else(move |e| {
        e.log();
        match e {
            BrokerError::Provider(_)
                | BrokerError::ProviderCancelled
                => bridges::email::auth(&ctx_handle2, &email_addr),
            _ => Box::new(future::err(e))
        }
    });

    Box::new(f)
}
