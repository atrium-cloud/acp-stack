# Deferred dependency bumps

- [ ] Bump `tower-http` to 0.7 once a `reqwest` release after 0.13.5 depends on it, so the tree carries one copy. Our CORS layer then derives `Vary` from its config instead of always naming the request-method and request-headers entries.
- [ ] Bump `tokio-tungstenite` to 0.30 once an `axum` release after 0.8.9 depends on it for its `ws` feature, so the tree carries one copy of tokio-tungstenite, tungstenite and sha1.
