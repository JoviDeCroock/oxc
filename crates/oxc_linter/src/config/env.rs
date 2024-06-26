use rustc_hash::FxHashMap;
use schemars::JsonSchema;
use serde::Deserialize;

/// Predefine global variables.
// TODO: list the keys we support
// <https://eslint.org/docs/v8.x/use/configure/language-options#specifying-environments>
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ESLintEnv(FxHashMap<String, bool>);

impl ESLintEnv {
    pub fn from_vec(env: Vec<String>) -> Self {
        let map = env.into_iter().map(|key| (key, true)).collect();

        Self(map)
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> + '_ {
        // Filter out false values
        self.0.iter().filter(|(_, v)| **v).map(|(k, _)| k.as_str())
    }
}

impl Default for ESLintEnv {
    fn default() -> Self {
        let mut map = FxHashMap::default();
        map.insert("builtin".to_string(), true);

        Self(map)
    }
}

#[cfg(test)]
mod test {
    use super::ESLintEnv;
    use itertools::Itertools;
    use serde::Deserialize;

    #[test]
    fn test_parse_env() {
        let env = ESLintEnv::deserialize(&serde_json::json!({
            "browser": true, "node": true, "es6": false
        }))
        .unwrap();
        assert_eq!(env.iter().count(), 2);
        assert!(env.iter().contains(&"browser"));
        assert!(env.iter().contains(&"node"));
        assert!(!env.iter().contains(&"es6"));
        assert!(!env.iter().contains(&"builtin"));
    }
    #[test]
    fn test_parse_env_default() {
        let env = ESLintEnv::default();
        assert_eq!(env.iter().count(), 1);
        assert!(env.iter().contains(&"builtin"));
    }
}
