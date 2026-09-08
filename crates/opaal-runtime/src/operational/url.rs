//! Closed HTTP/HTTPS URL parsing and normalization.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Url {
    scheme: Scheme,
    host: String,
    port: u16,
    path_and_query: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Scheme {
    Http,
    Https,
}

impl Url {
    pub fn parse(input: &str) -> Result<Self, UrlError> {
        if input.contains('#') {
            return Err(UrlError("URL fragments are unsupported".to_owned()));
        }
        let (scheme, rest, default_port) = if let Some(rest) = input.strip_prefix("https://") {
            (Scheme::Https, rest, 443)
        } else if let Some(rest) = input.strip_prefix("http://") {
            (Scheme::Http, rest, 80)
        } else {
            return Err(UrlError("URL scheme must be http or https".to_owned()));
        };
        let split = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..split];
        if authority.is_empty() || authority.contains('@') {
            return Err(UrlError(
                "URL authority is missing or contains credentials".to_owned(),
            ));
        }
        let (host, port) = parse_authority(authority, default_port)?;
        let mut path = rest[split..].to_owned();
        if path.is_empty() {
            path.push('/');
        }
        if path.starts_with('?') {
            path.insert(0, '/');
        }
        path = normalize_path_and_query(&path)?;
        Ok(Self {
            scheme,
            host,
            port,
            path_and_query: path,
        })
    }

    #[must_use]
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
    #[must_use]
    pub fn path_and_query(&self) -> &str {
        &self.path_and_query
    }
    #[must_use]
    pub fn is_literal_loopback(&self) -> bool {
        self.host
            .parse::<Ipv4Addr>()
            .is_ok_and(|value| value.is_loopback())
            || self
                .host
                .parse::<Ipv6Addr>()
                .is_ok_and(|value| value.is_loopback())
    }

    #[must_use]
    pub fn render(&self) -> String {
        let scheme = match self.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        let default = matches!(
            (self.scheme, self.port),
            (Scheme::Http, 80) | (Scheme::Https, 443)
        );
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if default {
            format!("{scheme}://{host}{}", self.path_and_query)
        } else {
            format!("{scheme}://{host}:{}{}", self.port, self.path_and_query)
        }
    }
}

fn parse_authority(authority: &str, default_port: u16) -> Result<(String, u16), UrlError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| UrlError("invalid bracketed IPv6 host".to_owned()))?;
        let host = &rest[..end];
        let host = host
            .parse::<Ipv6Addr>()
            .map_err(|_| UrlError("invalid IPv6 host".to_owned()))?;
        let suffix = &rest[end + 1..];
        let port = if suffix.is_empty() {
            default_port
        } else {
            parse_port(
                suffix
                    .strip_prefix(':')
                    .ok_or_else(|| UrlError("invalid IPv6 authority".to_owned()))?,
            )?
        };
        return Ok((host.to_string(), port));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, parse_port(port)?),
        _ => (authority, default_port),
    };
    if host.is_empty() {
        return Err(UrlError("invalid DNS or IPv4 host".to_owned()));
    }
    if let Ok(address) = host.parse::<Ipv4Addr>() {
        return Ok((address.to_string(), port));
    }
    if host
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Err(UrlError("invalid IPv4 host".to_owned()));
    }
    let host = host.to_ascii_lowercase();
    if host.len() > 253
        || host.ends_with('.')
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
    {
        return Err(UrlError("invalid DNS or IPv4 host".to_owned()));
    }
    Ok((host, port))
}

fn normalize_path_and_query(value: &str) -> Result<String, UrlError> {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_control() || matches!(byte, b' ' | b'\\' | b'"' | b'<' | b'>' | b'`') {
            return Err(UrlError(
                "URL path/query contains an unsafe unescaped byte".to_owned(),
            ));
        }
        if byte == b'%' {
            let high = *bytes
                .get(index + 1)
                .ok_or_else(|| UrlError("URL contains an incomplete percent escape".to_owned()))?;
            let low = *bytes
                .get(index + 2)
                .ok_or_else(|| UrlError("URL contains an incomplete percent escape".to_owned()))?;
            if !high.is_ascii_hexdigit() || !low.is_ascii_hexdigit() {
                return Err(UrlError(
                    "URL contains an invalid percent escape".to_owned(),
                ));
            }
            let decoded = (hex_value(high) << 4) | hex_value(low);
            if decoded.is_ascii_control()
                || matches!(decoded, b' ' | b'\\' | b'"' | b'<' | b'>' | b'`')
            {
                return Err(UrlError(
                    "URL contains a percent-encoded unsafe byte".to_owned(),
                ));
            }
            output.push('%');
            output.push(char::from(high.to_ascii_uppercase()));
            output.push(char::from(low.to_ascii_uppercase()));
            index += 3;
        } else {
            let character = value[index..]
                .chars()
                .next()
                .expect("index remains at a character boundary");
            output.push(character);
            index += character.len_utf8();
        }
    }
    Ok(output)
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

fn parse_port(text: &str) -> Result<u16, UrlError> {
    let port = text
        .parse::<u16>()
        .map_err(|_| UrlError("invalid URL port".to_owned()))?;
    if port == 0 {
        Err(UrlError("URL port cannot be zero".to_owned()))
    } else {
        Ok(port)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UrlError(String);
impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UrlError {}
