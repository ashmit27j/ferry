//! Certificates for the controller-to-proxy stream (ADR 0033).
//!
//! `ferry certgen` runs as a Helm hook Job before the controller starts. It
//! creates one CA, a server certificate for the controller Service, and one
//! client certificate shared by every proxy, and stores them in two Secrets.
//! Existing Secrets are kept unless they expire within [`RENEW_BEFORE_DAYS`],
//! so `helm upgrade` renews them in time.

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, date_time_ymd,
};

/// How long certificates are valid.
pub const VALIDITY_YEARS: i32 = 3;

/// Certificates expiring sooner than this are renewed.
pub const RENEW_BEFORE_DAYS: i64 = 90;

/// The client certificate's common name.
pub const PROXY_CLIENT_NAME: &str = "ferry-proxy";

/// A freshly generated set of certificates, as PEM.
#[derive(Debug, Clone)]
pub struct Bundle {
    /// The CA certificate (both sides trust it).
    pub ca_cert: String,
    /// The controller's server certificate.
    pub server_cert: String,
    /// Its key.
    pub server_key: String,
    /// The proxies' client certificate.
    pub client_cert: String,
    /// Its key.
    pub client_key: String,
    /// Expiry as days since the Unix epoch, for renewal decisions.
    pub not_after_days: i64,
}

/// Days since 1970-01-01 to a civil (year, month, day), proleptic Gregorian
/// (Howard Hinnant's algorithm).
#[must_use]
pub fn civil_from_days(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        i32::try_from(year).unwrap_or(i32::MAX),
        u8::try_from(month).unwrap_or(1),
        u8::try_from(day).unwrap_or(1),
    )
}

/// Days since the Unix epoch, now.
#[must_use]
pub fn today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs() / 86_400).unwrap_or(0))
}

/// Sets validity from yesterday (clock skew) for [`VALIDITY_YEARS`]; returns
/// the expiry in days since the epoch.
fn validity(params: &mut CertificateParams, today: i64) -> i64 {
    let (y, m, d) = civil_from_days(today - 1);
    // Day 28 exists in every month, so the dates below are always valid.
    let d = d.min(28);
    params.not_before = date_time_ymd(y, m, d);
    params.not_after = date_time_ymd(y + VALIDITY_YEARS, m, d);
    today - 1 + i64::from(VALIDITY_YEARS) * 365
}

/// Generates a CA, a server certificate for `server_names`, and a proxy
/// client certificate.
///
/// # Errors
///
/// Fails if a name is not a valid DNS name or key generation fails.
pub fn generate(server_names: &[String]) -> Result<Bundle, rcgen::Error> {
    let today = today();
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "ferry-snapshot-ca");
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let not_after_days = validity(&mut ca_params, today);
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate()?)?;

    let leaf = |names: Vec<String>, cn: &str, usage: ExtendedKeyUsagePurpose| {
        let mut p = CertificateParams::new(names)?;
        p.distinguished_name.push(DnType::CommonName, cn);
        p.extended_key_usages = vec![usage];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        validity(&mut p, today);
        let key = KeyPair::generate()?;
        let cert = p.signed_by(&key, &ca)?;
        Ok::<_, rcgen::Error>((cert.pem(), key.serialize_pem()))
    };
    let (server_cert, server_key) = leaf(
        server_names.to_vec(),
        server_names
            .first()
            .map_or("ferry-controller", String::as_str),
        ExtendedKeyUsagePurpose::ServerAuth,
    )?;
    let (client_cert, client_key) = leaf(
        vec![PROXY_CLIENT_NAME.to_owned()],
        PROXY_CLIENT_NAME,
        ExtendedKeyUsagePurpose::ClientAuth,
    )?;
    Ok(Bundle {
        ca_cert: ca.pem(),
        server_cert,
        server_key,
        client_cert,
        client_key,
        not_after_days,
    })
}

/// Whether certificates expiring on `not_after_days` should be renewed now.
#[must_use]
pub fn needs_renewal(not_after_days: i64) -> bool {
    not_after_days - today() < RENEW_BEFORE_DAYS
}

/// Annotation recording a Secret's certificate expiry (days since the epoch).
pub const NOT_AFTER_ANNOTATION: &str = "gateway.ferry.dev/not-after-days";

fn tls_secret(
    name: &str,
    ca: &str,
    cert: &str,
    key: &str,
    not_after_days: i64,
) -> k8s_openapi::api::core::v1::Secret {
    use k8s_openapi::ByteString;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    let bytes = |s: &str| ByteString(s.as_bytes().to_vec());
    k8s_openapi::api::core::v1::Secret {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            labels: Some(
                [(
                    "app.kubernetes.io/managed-by".to_owned(),
                    "ferry-certgen".to_owned(),
                )]
                .into(),
            ),
            annotations: Some(
                [(NOT_AFTER_ANNOTATION.to_owned(), not_after_days.to_string())].into(),
            ),
            ..ObjectMeta::default()
        },
        type_: Some("kubernetes.io/tls".into()),
        data: Some(
            [
                ("ca.crt".to_owned(), bytes(ca)),
                ("tls.crt".to_owned(), bytes(cert)),
                ("tls.key".to_owned(), bytes(key)),
            ]
            .into(),
        ),
        ..Default::default()
    }
}

/// Creates or renews the controller and proxy Secrets in `namespace`.
/// Returns whether new certificates were issued.
///
/// # Errors
///
/// Fails if the API calls fail or the names are invalid.
pub async fn ensure(
    client: kube::Client,
    namespace: &str,
    controller_secret: &str,
    proxy_secret: &str,
    server_names: &[String],
) -> Result<bool, crate::ControllerError> {
    use k8s_openapi::api::core::v1::Secret;
    use kube::api::{Api, Patch, PatchParams};
    let api: Api<Secret> = Api::namespaced(client, namespace);
    let expiry = |s: &Secret| {
        s.metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(NOT_AFTER_ANNOTATION))
            .and_then(|v| v.parse::<i64>().ok())
    };
    let current = api.get_opt(controller_secret).await?;
    let proxy = api.get_opt(proxy_secret).await?;
    if let (Some(c), Some(p)) = (&current, &proxy)
        && let (Some(a), Some(b)) = (expiry(c), expiry(p))
        && !needs_renewal(a.min(b))
    {
        tracing::info!(namespace, "certificates are current");
        return Ok(false);
    }
    let b = generate(server_names)?;
    let params = PatchParams::apply("ferry-certgen").force();
    for (name, cert, key) in [
        (controller_secret, &b.server_cert, &b.server_key),
        (proxy_secret, &b.client_cert, &b.client_key),
    ] {
        let doc = tls_secret(name, &b.ca_cert, cert, key, b.not_after_days);
        api.patch(name, &params, &Patch::Apply(&doc)).await?;
    }
    tracing::info!(
        namespace,
        controller_secret,
        proxy_secret,
        "issued certificates"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_739), (2026, 10, 13));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn bundle_has_a_ca_server_and_client() {
        let b = generate(&["ferry-controller.ferry-system.svc".into()]).unwrap();
        for pem in [&b.ca_cert, &b.server_cert, &b.client_cert] {
            assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
        }
        assert!(b.server_key.contains("PRIVATE KEY"));
        assert!(!needs_renewal(b.not_after_days));
        assert!(needs_renewal(today() + 30));
    }
}
