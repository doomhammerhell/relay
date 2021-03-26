initSidebarItems({"constant":[["PROTOCOL_VERSION","The latest version of the protocol."]],"enum":[["DataCategory","Classifies the type of data that is being ingested."],["EventType","The type of an event."],["LazyCellRef","A wrapper around the lazycell's value. This can be `Owned` if we enter race conditions when initializing the lazycell state and compute `T` multiple times."],["MonotonicResult","The conversion result of [`UnixTimestamp::to_instant`]."],["ParseAuthError","Represents an auth header parsing error."],["ParseDsnError","Represents a dsn url parsing error."],["ParseProjectIdError","Raised if a project ID cannot be parsed from a string."],["Scheme","Represents the scheme of an url http/https."],["SpanStatus","Trace status."]],"fn":[["glob_match","Performs a glob operation."],["glob_match_bytes","Performs a glob operation on bytes."],["instant_to_date_time","Converts an `Instant` into a `DateTime`."],["instant_to_system_time","Converts an `Instant` into a `SystemTime`."]],"macro":[["clone","A cloning alternative to a `move` closure."],["impl_str_de","Helper macro to implement string based deserialization."],["impl_str_ser","Helper macro to implement string based serialization."],["impl_str_serde","Helper macro to implement string based serialization and deserialization."],["metric","Emits a metric."],["tryf","Same as `try` but to be used in functions that return `Box<Future>` instead of `Result`."]],"mod":[["metrics","A high-level metric client built on cadence."]],"struct":[["Auth","Represents an auth header."],["Dsn","Represents a Sentry dsn."],["Glob","A simple glob matcher."],["GlobMatcher","Helper for glob matching"],["GlobOptions","Controls the options of the globber."],["ParseEventTypeError","An error used when parsing `EventType`."],["ParseProjectKeyError","An error parsing [`ProjectKey`]."],["ParseSpanStatusError","Error parsing a `SpanStatus`."],["ParseUnixTimestampError","An error returned from parsing [`UnixTimestamp`]."],["ProjectId","Represents a project ID."],["ProjectKey","The public key used in a DSN to identify and authenticate for a project at Sentry."],["RetryBackoff","A retry interval generator that increases timeouts with exponential backoff."],["UnixTimestamp","A unix timestamp (full seconds elapsed since 1970-01-01 00:00 UTC)."],["UpsertingLazyCell","An `AtomicLazyCell` acting as a kind of cached property."],["Uuid","A Universally Unique Identifier (UUID)."]]});