use crate::{
    bson::{doc, Document},
    client::options::ServerAddress,
    coll::options::FindOptions,
    error::{
        BulkWriteError,
        BulkWriteFailure,
        CommandError,
        Error,
        ErrorKind,
        WriteConcernError,
        WriteError,
        WriteFailure,
    },
    sdam::{ServerDescription, TopologyDescription},
    selection_criteria::{
        HedgedReadOptions,
        ReadPreference,
        ReadPreferenceOptions,
        SelectionCriteria,
    },
    test::{
        log_uncaptured,
        run_spec_test_with_path,
        spec::run_unified_format_test,
        TestClient,
        CLIENT_OPTIONS,
        DEFAULT_GLOBAL_TRACING_HANDLER,
        LOCK,
        SERVER_API,
    },
    trace::{
        command::{truncate_on_char_boundary, DEFAULT_MAX_DOCUMENT_LENGTH_BYTES},
        TracingRepresentation,
        COMMAND_TRACING_EVENT_TARGET,
    },
    TopologyType,
};
use std::{collections::HashMap, iter, sync::Arc, time::Duration};

use super::{run_unified_format_test_filtered, unified_runner::TestCase};

#[test]
fn tracing_truncation() {
    let two_emoji = String::from("🤔🤔");

    let mut s = two_emoji.clone();
    assert_eq!(s.len(), 8);

    // start of string is a boundary, so we should truncate there
    truncate_on_char_boundary(&mut s, 0);
    assert_eq!(s, String::from("..."));

    // we should "round up" to the end of the first emoji
    s = two_emoji.clone();
    truncate_on_char_boundary(&mut s, 1);
    assert_eq!(s, String::from("🤔..."));

    // 4 is a boundary, so we should truncate there
    s = two_emoji.clone();
    truncate_on_char_boundary(&mut s, 4);
    assert_eq!(s, String::from("🤔..."));

    // we should round up to the full string
    s = two_emoji.clone();
    truncate_on_char_boundary(&mut s, 5);
    assert_eq!(s, two_emoji);

    // end of string is a boundary, so we should truncate there
    s = two_emoji.clone();
    truncate_on_char_boundary(&mut s, 8);
    assert_eq!(s, two_emoji);

    // we should get the full string back if the new length is longer than the original
    s = two_emoji.clone();
    truncate_on_char_boundary(&mut s, 10);
    assert_eq!(s, two_emoji);
}

/// Prose test 1: Default truncation limit
#[cfg_attr(feature = "tokio-runtime", tokio::test)]
#[cfg_attr(feature = "async-std-runtime", async_std::test)]
async fn command_logging_truncation_default_limit() {
    let _guard = LOCK.run_exclusively().await;
    let client = TestClient::new().await;
    let coll = client.init_db_and_coll("tracing_test", "truncation").await;

    let _levels_guard = DEFAULT_GLOBAL_TRACING_HANDLER.set_levels(HashMap::from([(
        COMMAND_TRACING_EVENT_TARGET.to_string(),
        tracing::Level::DEBUG,
    )]));
    let mut tracing_subscriber = DEFAULT_GLOBAL_TRACING_HANDLER.subscribe();

    let docs = iter::repeat(doc! { "x": "y" }).take(100);
    coll.insert_many(docs, None)
        .await
        .expect("insert many should succeed");

    let events = tracing_subscriber
        .collect_events(Duration::from_millis(500), |_| true)
        .await;
    assert_eq!(events.len(), 2);

    let started = &events[0];
    let command = started.get_value_as_string("command");
    assert_eq!(command.len(), DEFAULT_MAX_DOCUMENT_LENGTH_BYTES + 3); // +3 for trailing "..."

    let succeeded = &events[1];
    let reply = succeeded.get_value_as_string("reply");
    assert!(reply.len() <= DEFAULT_MAX_DOCUMENT_LENGTH_BYTES + 3); // +3 for trailing "..."

    coll.find(None, None).await.expect("find should succeed");
    let succeeded = tracing_subscriber
        .wait_for_event(Duration::from_millis(500), |e| {
            e.get_value_as_string("message") == "Command succeeded"
        })
        .await
        .unwrap();
    let reply = succeeded.get_value_as_string("reply");
    assert_eq!(reply.len(), DEFAULT_MAX_DOCUMENT_LENGTH_BYTES + 3); // +3 for trailing "..."
}

/// Prose test 2: explicitly configured truncation limit
#[cfg_attr(feature = "tokio-runtime", tokio::test)]
#[cfg_attr(feature = "async-std-runtime", async_std::test)]
async fn command_logging_truncation_explicit_limit() {
    let _guard = LOCK.run_exclusively().await;

    let mut client_opts = CLIENT_OPTIONS.get().await.clone();
    client_opts.tracing_max_document_length_bytes = Some(5);
    let client = TestClient::with_options(Some(client_opts)).await;

    let _levels_guard = DEFAULT_GLOBAL_TRACING_HANDLER.set_levels(HashMap::from([(
        COMMAND_TRACING_EVENT_TARGET.to_string(),
        tracing::Level::DEBUG,
    )]));
    let mut tracing_subscriber = DEFAULT_GLOBAL_TRACING_HANDLER.subscribe();

    client
        .database("tracing_test")
        .run_command(doc! { "hello" : "true" }, None)
        .await
        .expect("hello command should succeed");

    let events = tracing_subscriber
        .collect_events(Duration::from_millis(500), |_| true)
        .await;
    assert_eq!(events.len(), 2);

    let started = &events[0];
    let command = started.get_value_as_string("command");
    assert_eq!(command.len(), 8); // 5 + 3 for trailing "..."

    let succeeded = &events[1];
    let reply = succeeded.get_value_as_string("reply");
    assert_eq!(reply.len(), 8); // 5 + 3 for trailing "..."

    // TODO RUST-1405: when we expose the full server reply for command errors, we should confirm
    // that gets correctly truncated in command failed events here as well.
}

/// Prose test 3: mid-codepoint truncation
#[cfg_attr(feature = "tokio-runtime", tokio::test)]
#[cfg_attr(feature = "async-std-runtime", async_std::test)]
async fn command_logging_truncation_mid_codepoint() {
    let _guard = LOCK.run_exclusively().await;

    let mut client_opts = CLIENT_OPTIONS.get().await.clone();
    client_opts.tracing_max_document_length_bytes = Some(215);
    let client = TestClient::with_options(Some(client_opts)).await;
    // On non-standalone topologies the command includes a clusterTime and so gets truncated
    // differently.
    if !client.is_standalone() {
        log_uncaptured("Skipping test due to incompatible topology type");
        return;
    }
    // Truncation happens differently when a server API version is included in the command.
    if SERVER_API.is_some() {
        log_uncaptured("Skipping test due to server API version being specified");
        return;
    }

    let coll = client.init_db_and_coll("tracing_test", "truncation").await;

    let _levels_guard = DEFAULT_GLOBAL_TRACING_HANDLER.set_levels(HashMap::from([(
        COMMAND_TRACING_EVENT_TARGET.to_string(),
        tracing::Level::DEBUG,
    )]));
    let mut tracing_subscriber = DEFAULT_GLOBAL_TRACING_HANDLER.subscribe();

    let docs = iter::repeat(doc! { "🤔": "🤔🤔🤔🤔🤔🤔" }).take(10);
    coll.insert_many(docs, None)
        .await
        .expect("insert many should succeed");

    let started = tracing_subscriber
        .wait_for_event(Duration::from_millis(500), |e| {
            e.get_value_as_string("message") == "Command started"
        })
        .await
        .unwrap();

    let command = started.get_value_as_string("command");

    // 215 falls in the middle of an emoji (each is 4 bytes), so we should round up to 218, + 3 for
    // trailing "..."
    assert_eq!(command.len(), 221);

    let find_options = FindOptions::builder()
        .projection(doc! { "_id": 0, "🤔": 1 })
        .build();
    coll.find(None, find_options)
        .await
        .expect("find should succeed");
    let succeeded = tracing_subscriber
        .wait_for_event(Duration::from_millis(500), |e| {
            e.get_value_as_string("message") == "Command succeeded"
                && e.get_value_as_string("commandName") == "find"
        })
        .await
        .unwrap();
    let reply = succeeded.get_value_as_string("reply");
    // 215 falls in the middle of an emoji (each is 4 bytes), so we should round up to 218, + 3 for
    // trailing "..."
    assert_eq!(reply.len(), 221);
}

#[test]
fn error_redaction() {
    fn assert_is_redacted(error: Error) {
        fn assert_on_properties(
            code: i32,
            code_name: String,
            message: String,
            details: Option<Document>,
        ) {
            assert!(code != 0, "Error code should be non-zero");
            assert!(!code_name.is_empty(), "Error code name should be non-empty");
            assert!(
                !code_name.contains("REDACTED"),
                "Error code name should not be redacted"
            );
            assert!(message == "REDACTED", "Error message should be redacted");
            assert!(details.is_none(), "Error details should be redacted");
        }

        match *error.kind {
            ErrorKind::Command(CommandError {
                code,
                code_name,
                message,
                ..
            }) => {
                assert_on_properties(code, code_name, message, None);
            }
            ErrorKind::Write(write_failure) => match write_failure {
                WriteFailure::WriteConcernError(WriteConcernError {
                    code,
                    code_name,
                    message,
                    details,
                    ..
                }) => {
                    assert_on_properties(code, code_name, message, details);
                }
                WriteFailure::WriteError(WriteError {
                    code,
                    code_name,
                    message,
                    details,
                }) => {
                    assert_on_properties(code, code_name.unwrap(), message, details);
                }
            },
            ErrorKind::BulkWrite(BulkWriteFailure {
                write_errors,
                write_concern_error,
                ..
            }) => {
                if let Some(write_errors) = write_errors {
                    for BulkWriteError {
                        code,
                        code_name,
                        message,
                        details,
                        ..
                    } in write_errors
                    {
                        assert_on_properties(code, code_name.unwrap(), message, details);
                    }
                }
                if let Some(WriteConcernError {
                    code,
                    code_name,
                    message,
                    details,
                    ..
                }) = write_concern_error
                {
                    assert_on_properties(code, code_name, message, details);
                }
            }
            _ => {}
        }
    }

    let labels: Option<Vec<_>> = None;

    let mut command_error = Error::new(
        ErrorKind::Command(CommandError {
            code: 123,
            code_name: "CodeName".to_string(),
            message: "Hello".to_string(),
            topology_version: None,
        }),
        labels.clone(),
    );
    command_error.redact();
    assert_is_redacted(command_error);

    let wce = WriteConcernError {
        code: 123,
        code_name: "CodeName".to_string(),
        message: "Hello".to_string(),
        details: Some(doc! { "x" : 1}),
        labels: vec![],
    };
    let wce_copy = wce.clone();

    let mut write_concern_error = Error::new(
        ErrorKind::Write(WriteFailure::WriteConcernError(wce)),
        labels.clone(),
    );
    write_concern_error.redact();
    assert_is_redacted(write_concern_error);

    let mut write_error = Error::new(
        ErrorKind::Write(WriteFailure::WriteError(WriteError {
            code: 123,
            code_name: Some("CodeName".to_string()),
            message: "Hello".to_string(),
            details: Some(doc! { "x" : 1}),
        })),
        labels.clone(),
    );
    write_error.redact();
    assert_is_redacted(write_error);

    let mut bulk_write_error = Error::new(
        ErrorKind::BulkWrite(BulkWriteFailure {
            write_errors: Some(vec![BulkWriteError {
                index: 0,
                code: 123,
                code_name: Some("CodeName".to_string()),
                message: "Hello".to_string(),
                details: Some(doc! { "x" : 1}),
            }]),
            write_concern_error: Some(wce_copy),
            inserted_ids: HashMap::default(),
        }),
        labels,
    );
    bulk_write_error.redact();
    assert_is_redacted(bulk_write_error);
}

#[test]
fn selection_criteria_tracing_representation() {
    assert_eq!(
        SelectionCriteria::ReadPreference(ReadPreference::Primary).tracing_representation(),
        "ReadPreference { Mode: Primary }"
    );

    // non-primary read preferences with empty options - options should be omitted from
    // representation.
    let empty_opts = ReadPreferenceOptions::builder().build();

    assert_eq!(
        SelectionCriteria::ReadPreference(ReadPreference::PrimaryPreferred {
            options: empty_opts.clone()
        })
        .tracing_representation(),
        "ReadPreference { Mode: PrimaryPreferred }"
    );
    assert_eq!(
        SelectionCriteria::ReadPreference(ReadPreference::Secondary {
            options: empty_opts.clone()
        })
        .tracing_representation(),
        "ReadPreference { Mode: Secondary }"
    );
    assert_eq!(
        SelectionCriteria::ReadPreference(ReadPreference::SecondaryPreferred {
            options: empty_opts.clone()
        })
        .tracing_representation(),
        "ReadPreference { Mode: SecondaryPreferred }"
    );
    assert_eq!(
        SelectionCriteria::ReadPreference(ReadPreference::Nearest {
            options: empty_opts
        })
        .tracing_representation(),
        "ReadPreference { Mode: Nearest }"
    );

    let mut tag_set = HashMap::new();
    tag_set.insert("a".to_string(), "b".to_string());
    let opts_with_tag_sets = ReadPreferenceOptions::builder()
        .tag_sets(vec![tag_set.clone()])
        .build();

    assert_eq!(
        SelectionCriteria::ReadPreference(ReadPreference::PrimaryPreferred {
            options: opts_with_tag_sets
        })
        .tracing_representation(),
        "ReadPreference { Mode: PrimaryPreferred, Tag Sets: [{\"a\": \"b\"}] }"
    );

    let opts_with_max_staleness = ReadPreferenceOptions::builder()
        .max_staleness(Duration::from_millis(200))
        .build();
    assert_eq!(
        SelectionCriteria::ReadPreference(ReadPreference::PrimaryPreferred {
            options: opts_with_max_staleness
        })
        .tracing_representation(),
        "ReadPreference { Mode: PrimaryPreferred, Max Staleness: 200ms }"
    );

    let opts_with_hedge = ReadPreferenceOptions::builder()
        .hedge(HedgedReadOptions::with_enabled(true))
        .build();
    assert_eq!(
        SelectionCriteria::ReadPreference(ReadPreference::PrimaryPreferred {
            options: opts_with_hedge
        })
        .tracing_representation(),
        "ReadPreference { Mode: PrimaryPreferred, Hedge: true }"
    );

    let opts_with_multiple_options = ReadPreferenceOptions::builder()
        .max_staleness(Duration::from_millis(200))
        .tag_sets(vec![tag_set])
        .build();
    assert_eq!(
        SelectionCriteria::ReadPreference(ReadPreference::PrimaryPreferred {
            options: opts_with_multiple_options
        })
        .tracing_representation(),
        "ReadPreference { Mode: PrimaryPreferred, Tag Sets: [{\"a\": \"b\"}], Max Staleness: \
         200ms }"
    );

    assert_eq!(
        SelectionCriteria::Predicate(Arc::new(|_s| true)).tracing_representation(),
        "Custom predicate"
    );
}

#[test]
fn topology_description_tracing_representation() {
    let mut servers = HashMap::new();
    servers.insert(
        ServerAddress::default(),
        ServerDescription::new(ServerAddress::default()),
    );

    let oid = bson::oid::ObjectId::new();
    let description = TopologyDescription {
        single_seed: false,
        set_name: Some("myReplicaSet".to_string()),
        topology_type: TopologyType::ReplicaSetWithPrimary,
        max_set_version: Some(100),
        max_election_id: Some(oid),
        compatibility_error: Some("Compat error".to_string()),
        session_support_status: crate::sdam::SessionSupportStatus::default(),
        transaction_support_status: crate::sdam::TransactionSupportStatus::default(),
        cluster_time: None,
        local_threshold: None,
        heartbeat_freq: None,
        servers,
    };

    assert_eq!(
        description.tracing_representation(),
        format!(
            "{{ Type: ReplicaSetWithPrimary, Set Name: myReplicaSet, Max Set Version: 100, Max \
             Election ID: {}, Compatibility Error: Compat error, Servers: [ {{ Address: \
             localhost:27017, Type: Unknown }} ] }}",
            oid.to_hex()
        ),
    )
}

#[cfg_attr(feature = "tokio-runtime", tokio::test(flavor = "multi_thread"))]
#[cfg_attr(feature = "async-std-runtime", async_std::test)]
async fn command_logging_unified() {
    let _guard = LOCK.run_exclusively().await;
    run_spec_test_with_path(
        &["command-logging-and-monitoring", "logging"],
        run_unified_format_test,
    )
    .await;
}

#[cfg_attr(feature = "tokio-runtime", tokio::test(flavor = "multi_thread"))]
#[cfg_attr(feature = "async-std-runtime", async_std::test)]
async fn connection_logging_unified() {
    let test_predicate = |tc: &TestCase|
        // TODO: RUST-1096 Unskip when configurable maxConnecting is added.
        tc.description != "maxConnecting should be included in connection pool created message when specified" &&
        // We don't support any of these options (and are unlikely to ever support them).
        tc.description != "waitQueueTimeoutMS should be included in connection pool created message when specified" &&
        tc.description != "waitQueueSize should be included in connection pool created message when specified" &&
        tc.description != "waitQueueMultiple should be included in connection pool created message when specified";

    let _guard = LOCK.run_exclusively().await;

    run_spec_test_with_path(
        &["connection-monitoring-and-pooling", "logging"],
        |path, file| run_unified_format_test_filtered(path, file, test_predicate),
    )
    .await;
}

#[cfg_attr(feature = "tokio-runtime", tokio::test(flavor = "multi_thread"))]
#[cfg_attr(feature = "async-std-runtime", async_std::test)]
async fn server_selection_logging_unified() {
    let test_predicate = |tc: &TestCase|
        // TODO: RUST-583 Unskip these if/when we add operation IDs as part of bulkWrite support.
        tc.description != "Successful bulkWrite operation: log messages have operationIds" &&
        tc.description != "Failed bulkWrite operation: log messages have operationIds" &&
        // TODO: RUST-1585 Unskip these tests (they are currently flaky on certain topologies due to performing extra
        // server selections to check for support.)
        tc.description != "Failure due to unreachable server";

    let _guard = LOCK.run_exclusively().await;
    run_spec_test_with_path(&["server-selection", "logging"], |path, file| {
        run_unified_format_test_filtered(path, file, test_predicate)
    })
    .await;
}
