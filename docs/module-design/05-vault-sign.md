# rollball-vault + rollball-sign

## rollball-vault — 密钥加密存储

**定位**：集中管理 LLM API Key，加密存储，一次性分发。

```
crates/rollball-vault/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── vault.rs                   # Vault 主结构（open/store/retrieve）
    ├── encryption.rs              # ChaCha20-Poly1305 AEAD 加解密
    ├── key_derivation.rs          # 用户密码 → 主密钥派生（Argon2id）
    └── error.rs
```

### 关键 API

```rust
pub struct Vault {
    vault_dir: PathBuf,
    master_key: Option<SecretString>,  // 解锁后驻留内存
}

impl Vault {
    /// 创建或打开 Vault
    pub fn open(vault_dir: &Path) -> Result<Self>;
    
    /// 用密码解锁（派生主密钥）
    pub fn unlock(&mut self, password: &str) -> Result<()>;
    
    /// 存储密钥（加密后写入文件）
    pub fn store(&self, key_name: &str, secret: &str) -> Result<()>;
    
    /// 检索密钥（解密后返回 SecretString，零拷贝）
    pub fn retrieve(&self, key_name: &str) -> Result<SecretString>;
    
    /// 列出所有密钥名称（不返回值）
    pub fn list(&self) -> Result<Vec<String>>;
}
```

### 借鉴 ZeroClaw

- ZeroClaw 的 `security/secrets.rs` 实现了类似的加密存储，使用 `chacha20poly1305`
- Rollball 增加了密码派生主密钥的步骤（Argon2id），ZeroClaw 使用配置文件中的密钥

### 依赖

- `chacha20poly1305` — AEAD 加密
- `rand` — CSPRNG
- `secrecy` — SecretString 零拷贝封装
- `sha2`, `hmac` — 密钥派生

---

## rollball-sign — .agent 包签名/验签

**定位**：独立的签名工具链，提供 `rollball-keygen`、`rollball-sign`、`rollball-verify` 三个命令。

```
crates/rollball-sign/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── signing_block.rs           # Signing Block 数据结构
    ├── keygen.rs                  # 密钥对生成（Ed25519）
    ├── sign.rs                    # 签名（插入 Signing Block 到 ZIP）
    ├── verify.rs                  # 验签（提取 Signing Block + 校验摘要）
    ├── certificate.rs             # X.509 证书处理
    └── error.rs
```

### 关键数据结构

```rust
pub struct SigningBlock {
    pub signers: Vec<Signer>,
}

pub struct Signer {
    pub certificates: Vec<Certificate>,     // X.509 证书链
    pub digest_algorithm: DigestAlgorithm,  // SHA-256
    pub digests: Vec<SectionDigest>,        // 各 section 摘要
    pub signature: Vec<u8>,                 // 对 digests 的签名
    pub signed_attrs: SignedAttributes,     // 签名时间戳等
}

pub enum SignerIdentity {
    Developer,           // 自签名
    Platform,            // 平台签名（系统 Agent）
    CaIssued,            // CA 签发（商店 Agent）
}
```

### 依赖

- `ed25519-dalek` — Ed25519 签名
- `x509-cert` — X.509 证书
- `sha2` — SHA-256 摘要
- `zip` — ZIP 操作
- `clap` — CLI
