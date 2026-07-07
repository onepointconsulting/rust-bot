use std::path::PathBuf;

use rust_bot::agent::skills::SkillsLoader;



#[tokio::test]
async fn test_skills_loader_list_skills() {
    let loader = SkillsLoader::new(&PathBuf::from("."), None);
    let skills = loader.list_skills(false);
    println!("skills: {:?}", skills);
}

#[tokio::test]
async fn test_skills_loader_get_always_skills() {
    let loader = SkillsLoader::new(&PathBuf::from("."), None);
    let skills = loader.get_always_skills();
    println!("skills: {:?}", skills);
}