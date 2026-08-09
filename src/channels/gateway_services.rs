use crate::security::WebUIIngressPolicy;

#[derive(Clone)]
pub struct GatewayServices {
    pub ingress: WebUIIngressPolicy
}

impl Default for GatewayServices {
    fn default() -> Self {
        Self {
            ingress: WebUIIngressPolicy::default(),
        }
    }
}