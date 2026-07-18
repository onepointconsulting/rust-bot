use std::path::PathBuf;
use dotenv::dotenv;
use rust_bot::agent::tools::{base::Tool, docx::DocxConversionTool, filesystem::FsToolConfig};


#[tokio::test]
async fn test_docx_tool() {
    dotenv().ok();
    let fs_tool_config = FsToolConfig {
        workspace: Some(PathBuf::from("./")),
        allowed_dir: None,
        extra_allowed_dirs: None,
    };
    let docx_conversion_tool = DocxConversionTool::new(fs_tool_config);
    let output_path = "./docs/Newsletter_Brief.pdf";
    let result = docx_conversion_tool.execute(&serde_json::json!({
        "docx_path": "./docs/Newsletter_Brief.docx",
        "output_path": output_path,
    })).await;
    println!("{}", result);
    assert!(PathBuf::from(output_path).exists());
}