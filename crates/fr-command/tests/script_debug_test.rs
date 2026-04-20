use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;
use fr_command::CommandError;

#[test]
fn test_script_debug() {
    let mut store = Store::new();

    // Wrong arity
    let out = dispatch_argv(
        &[b"SCRIPT".to_vec(), b"DEBUG".to_vec()],
        &mut store,
        0,
    );
    assert_eq!(
        out.unwrap_err(),
        CommandError::WrongSubcommandArity {
            command: "SCRIPT",
            subcommand: "DEBUG".to_string(),
        }
    );

    // Invalid mode
    let out = dispatch_argv(
        &[b"SCRIPT".to_vec(), b"DEBUG".to_vec(), b"MAYBE".to_vec()],
        &mut store,
        0,
    );
    assert_eq!(
        out.unwrap_err(),
        CommandError::Custom("ERR Use SCRIPT DEBUG YES, SYNC or NO".to_string())
    );

    // Valid modes
    for mode in ["YES", "SYNC", "NO", "yes", "sync", "no"] {
        let out = dispatch_argv(
            &[b"SCRIPT".to_vec(), b"DEBUG".to_vec(), mode.as_bytes().to_vec()],
            &mut store,
            0,
        );
        assert_eq!(out.unwrap(), RespFrame::SimpleString("OK".to_string()));
    }
}
