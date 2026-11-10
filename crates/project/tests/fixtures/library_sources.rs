// Device-free project sources shared by storage and CLI fixtures.
pub const FIRST_OLD: &str =
    "FUNCTION_BLOCK FB_First\nVAR_OUTPUT n : INT; END_VAR\nn := 1;\nEND_FUNCTION_BLOCK\n";
pub const FIRST_NEW: &str =
    "FUNCTION_BLOCK FB_First\nVAR_OUTPUT n : INT; END_VAR\nn := 2;\nEND_FUNCTION_BLOCK\n";
pub const SECOND_OLD: &str =
    "FUNCTION_BLOCK FB_Second\nVAR_OUTPUT n : INT; END_VAR\nn := 10;\nEND_FUNCTION_BLOCK\n";
pub const SECOND_NEW: &str =
    "FUNCTION_BLOCK FB_Second\nVAR_OUTPUT n : INT; END_VAR\nn := 20;\nEND_FUNCTION_BLOCK\n";
pub const MAIN: &str = "PROGRAM main\nVAR a : FB_First; b : FB_Second; total : INT; END_VAR\na();\nb();\ntotal := a.n + b.n;\nEND_PROGRAM\n";
