use std::fmt;

/// One forecast point, independent of where it was stored.
///
/// `altitude_metres == None` means the request omits `asl` and Meteoblue's
/// returned metadata height becomes the ground altitude. `Some` requests an
/// explicit elevation and is checked against the response.
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    saved_name: String,
    display_name: String,
    latitude: f64,
    longitude: f64,
    altitude_metres: Option<i16>,
    timezone: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationError {
    EmptySavedName,
    EmptyDisplayName,
    InvalidLatitude,
    InvalidLongitude,
    InvalidAltitude,
    InvalidTimezone,
}

impl fmt::Display for LocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptySavedName => "location saved name is empty",
            Self::EmptyDisplayName => "location display name is empty",
            Self::InvalidLatitude => "location latitude must be between -90 and 90",
            Self::InvalidLongitude => "location longitude must be between -180 and 180",
            Self::InvalidAltitude => "location altitude must be between -500 and 9000 metres",
            Self::InvalidTimezone => "location timezone is not a supported IANA name",
        };
        f.write_str(message)
    }
}

impl std::error::Error for LocationError {}

impl Location {
    pub fn new(
        saved_name: impl Into<String>,
        display_name: impl Into<String>,
        latitude: f64,
        longitude: f64,
        altitude_metres: Option<i16>,
        timezone: impl Into<String>,
    ) -> Result<Self, LocationError> {
        let saved_name = saved_name.into();
        let display_name = display_name.into();
        let timezone = timezone.into();
        if saved_name.trim().is_empty() {
            return Err(LocationError::EmptySavedName);
        }
        if display_name.trim().is_empty() {
            return Err(LocationError::EmptyDisplayName);
        }
        if !latitude.is_finite() || !(-90.0..=90.0).contains(&latitude) {
            return Err(LocationError::InvalidLatitude);
        }
        if !longitude.is_finite() || !(-180.0..=180.0).contains(&longitude) {
            return Err(LocationError::InvalidLongitude);
        }
        if altitude_metres.is_some_and(|altitude| !(-500..=9000).contains(&altitude)) {
            return Err(LocationError::InvalidAltitude);
        }
        let valid_timezone = !timezone.is_empty()
            && timezone.len() <= 64
            && timezone.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'+')
            });
        if !valid_timezone {
            return Err(LocationError::InvalidTimezone);
        }
        Ok(Self {
            saved_name,
            display_name,
            latitude,
            longitude,
            altitude_metres,
            timezone,
        })
    }

    pub fn saved_name(&self) -> &str {
        &self.saved_name
    }
    pub fn display_name(&self) -> &str {
        &self.display_name
    }
    pub fn latitude(&self) -> f64 {
        self.latitude
    }
    pub fn longitude(&self) -> f64 {
        self.longitude
    }
    pub fn altitude_metres(&self) -> Option<i16> {
        self.altitude_metres
    }
    pub fn timezone(&self) -> &str {
        &self.timezone
    }
}

/// The built-in default until the SD-backed location repository exists.
pub fn default_location() -> Location {
    Location::new("leith", "Leith", 55.97, -3.17, None, "Europe/London")
        .expect("the built-in Leith location is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leith_lets_meteoblue_resolve_altitude() {
        let leith = default_location();
        assert_eq!(leith.saved_name(), "leith");
        assert_eq!(leith.latitude(), 55.97);
        assert_eq!(leith.longitude(), -3.17);
        assert_eq!(leith.altitude_metres(), None);
        assert_eq!(leith.timezone(), "Europe/London");
    }

    #[test]
    fn explicit_altitude_remains_supported() {
        let location = Location::new(
            "summit",
            "Summit",
            57.09,
            -3.65,
            Some(1047),
            "Europe/London",
        )
        .unwrap();
        assert_eq!(location.altitude_metres(), Some(1047));
    }
}
