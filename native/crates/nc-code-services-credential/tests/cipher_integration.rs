use nc_code_services_credential::cipher::{
    CredentialCipherProvider, CREDENTIAL_DECRYPT_ERROR_CODE, ENCRYPTED_VALUE_PREFIX,
};

#[test]
fn cipher_roundtrip_and_compat() {
    let c = CredentialCipherProvider::new("test-secret");
    let enc = c.encrypt("value").unwrap();
    assert!(enc.starts_with(ENCRYPTED_VALUE_PREFIX));
    assert_eq!(c.decrypt(&enc).unwrap(), "value");
    assert_eq!(c.decrypt("legacy-plain").unwrap(), "legacy-plain");
    let err = c.decrypt("enc:v1:not.valid.base64!!!").unwrap_err();
    assert_eq!(err.code, CREDENTIAL_DECRYPT_ERROR_CODE);
}
