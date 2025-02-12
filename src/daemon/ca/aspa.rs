//! Autonomous System Provider Authorization
//!
//! This is still being discussed in the IETF. No RFC just yet.
//! See the following drafts:
//! https://datatracker.ietf.org/doc/draft-ietf-sidrops-aspa-profile/
//! https://datatracker.ietf.org/doc/draft-ietf-sidrops-aspa-verification/
//!

use std::{collections::HashMap, fmt::Debug};

use rpki::{
    ca::publication::Base64,
    repository::{
        aspa::{Aspa, AspaBuilder},
        sigobj::SignedObjectBuilder,
        x509::{Serial, Time, Validity},
    },
    rrdp::Hash,
    uri,
};

use crate::{
    commons::{
        api::{AspaCustomer, AspaDefinition, AspaProvidersUpdate, ObjectName},
        crypto::KrillSigner,
        error::Error,
        KrillResult,
    },
    daemon::{
        ca::{AspaObjectsUpdates, CertifiedKey},
        config::{Config, IssuanceTimingConfig},
    },
};

pub fn make_aspa_object(
    aspa_def: AspaDefinition,
    certified_key: &CertifiedKey,
    validity: Validity,
    signer: &KrillSigner,
) -> KrillResult<Aspa> {
    let name = ObjectName::from(&aspa_def);

    let aspa_builder = {
        let (customer_as, providers) = aspa_def.unpack();
        AspaBuilder::new(customer_as, providers).map_err(|e| Error::Custom(format!("Cannot use aspa config: {}", e)))
    }?;

    let object_builder = {
        let incoming_cert = certified_key.incoming_cert();

        let crl_uri = incoming_cert.crl_uri();
        let aspa_uri = incoming_cert.uri_for_name(&name);
        let ca_issuer = incoming_cert.uri().clone();

        let mut object_builder =
            SignedObjectBuilder::new(signer.random_serial()?, validity, crl_uri, ca_issuer, aspa_uri);
        object_builder.set_issuer(Some(incoming_cert.subject().clone()));
        object_builder.set_signing_time(Some(Time::now()));

        object_builder
    };

    Ok(signer.sign_aspa(aspa_builder, object_builder, certified_key.key_id())?)
}

//------------ AspaDefinitions ---------------------------------------------

/// This type contains the ASPA definitions for a CA. Generally speaking
/// the [`AspaCustomer`] ASN will be held in a single [`ResourceClass`] only,
/// but at least in theory the CA could issue ASPA objects in each RC that
/// holds the ASN.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AspaDefinitions {
    attestations: HashMap<AspaCustomer, AspaDefinition>,
}

impl AspaDefinitions {
    // Add or replace a new definition
    pub fn add_or_replace(&mut self, aspa_def: AspaDefinition) {
        let customer = aspa_def.customer();
        self.attestations.insert(customer, aspa_def);
    }

    // Remove an existing definition (if it is present)
    pub fn remove(&mut self, customer: AspaCustomer) {
        self.attestations.remove(&customer);
    }

    // Applies an update. This assumes that the update was verified beforehand.
    pub fn apply_update(&mut self, customer: AspaCustomer, update: &AspaProvidersUpdate) {
        if let Some(current) = self.attestations.get_mut(&customer) {
            current.apply_update(update);

            // If there are no remaining providers for this AspaDefinition, then
            // remove it so that its ASPA object will also be removed.
            if current.providers().is_empty() {
                self.attestations.remove(&customer);
            }
        } else {
            // There was no AspaDefinition. So create an empty definition, apply
            // the update and then add it.
            let mut def = AspaDefinition::new(customer, vec![]);
            def.apply_update(update);

            self.attestations.insert(customer, def);
        }
    }

    pub fn all(&self) -> impl Iterator<Item = &AspaDefinition> {
        self.attestations.values()
    }
}

/// # Set operations
///
impl AspaDefinitions {
    pub fn get(&self, customer: AspaCustomer) -> Option<&AspaDefinition> {
        self.attestations.get(&customer)
    }

    pub fn has(&self, customer: AspaCustomer) -> bool {
        self.attestations.contains_key(&customer)
    }

    pub fn len(&self) -> usize {
        self.attestations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.attestations.is_empty()
    }
}

//------------ AspaObjects -------------------------------------------------

/// ASPA objects held by a resource class in a CA.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AspaObjects(HashMap<AspaCustomer, AspaInfo>);

impl AspaObjects {
    pub fn make_aspa(
        &self,
        aspa_def: AspaDefinition,
        certified_key: &CertifiedKey,
        issuance_timing: &IssuanceTimingConfig,
        signer: &KrillSigner,
    ) -> KrillResult<AspaInfo> {
        let aspa = make_aspa_object(
            aspa_def.clone(),
            certified_key,
            issuance_timing.new_aspa_validity(),
            signer,
        )?;
        Ok(AspaInfo::new_aspa(aspa_def, aspa))
    }

    /// Issue new ASPA objects based on configuration, and remove
    /// object for which the customer AS is no longer held.
    ///
    /// Note: we pass in *all* AspaDefinitions for the CA, not all
    ///   definitions will be relevant for the RC (key) holding
    ///   this AspaObjects.
    pub fn update(
        &self,
        all_aspa_defs: &AspaDefinitions,
        certified_key: &CertifiedKey,
        config: &Config,
        signer: &KrillSigner,
    ) -> KrillResult<AspaObjectsUpdates> {
        let mut object_updates = AspaObjectsUpdates::default();
        let resources = certified_key.incoming_cert().resources();

        // Issue new and updated ASPAs for definitions relevant to the resources in scope
        for relevant_aspa in all_aspa_defs
            .all()
            .filter(|aspa| resources.contains_asn(aspa.customer()))
        {
            let need_to_issue = self
                .0
                .get(&relevant_aspa.customer())
                .map(|existing| existing.definition() != relevant_aspa)
                .unwrap_or(true);

            if need_to_issue {
                let aspa_info =
                    self.make_aspa(relevant_aspa.clone(), certified_key, &config.issuance_timing, signer)?;
                object_updates.add_updated(aspa_info);
            }
        }

        // Check if any currently held ASPA object needs to be removed
        for customer in self.0.keys() {
            if !all_aspa_defs.has(*customer) || !resources.contains_asn(*customer) {
                // definition was removed, or it's overclaiming
                object_updates.add_removed(*customer);
            }
        }

        Ok(object_updates)
    }

    // Re-new ASPAs, if the renew_threshold is specified, then
    // only objects which will expire before that time will be
    // renewed.
    pub fn renew(
        &self,
        certified_key: &CertifiedKey,
        renew_threshold: Option<Time>,
        issuance_timing: &IssuanceTimingConfig,
        signer: &KrillSigner,
    ) -> KrillResult<AspaObjectsUpdates> {
        let mut updates = AspaObjectsUpdates::default();

        for aspa in self.0.values() {
            let renew = renew_threshold
                .map(|threshold| aspa.expires() < threshold)
                .unwrap_or(true); // always renew if no threshold is specified

            if renew {
                let aspa_definition = aspa.definition().clone();

                let new_aspa = self.make_aspa(aspa_definition, certified_key, issuance_timing, signer)?;
                updates.add_updated(new_aspa);
            }
        }

        Ok(updates)
    }

    pub fn updated(&mut self, updates: AspaObjectsUpdates) {
        let (updated, removed) = updates.unpack();
        for aspa_info in updated {
            let customer = aspa_info.customer();
            self.0.insert(customer, aspa_info);
        }
        for customer in removed {
            self.0.remove(&customer);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

//------------ AspaInfo ----------------------------------------------------

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AspaInfo {
    // The customer ASN and all Provider ASNs
    definition: AspaDefinition,

    // The validity time for this ASPA.
    validity: Validity,

    // The serial number (needed for revocation)
    serial: Serial,

    // The URI where this object is expected to be published
    uri: uri::Rsync,

    // The actual ASPA object in base64 format.
    base64: Base64,

    // The ASPA object's hash
    hash: Hash,
}

impl AspaInfo {
    pub fn new(definition: AspaDefinition, aspa: Aspa) -> Self {
        let validity = aspa.cert().validity();
        let serial = aspa.cert().serial_number();
        let uri = aspa.cert().signed_object().unwrap().clone(); // safe for our own ROAs
        let base64 = Base64::from(&aspa);
        let hash = base64.to_hash();

        AspaInfo {
            definition,
            validity,
            serial,
            uri,
            base64,
            hash,
        }
    }

    pub fn new_aspa(definition: AspaDefinition, aspa: Aspa) -> Self {
        AspaInfo::new(definition, aspa)
    }

    pub fn definition(&self) -> &AspaDefinition {
        &self.definition
    }

    pub fn customer(&self) -> AspaCustomer {
        self.definition.customer()
    }

    pub fn expires(&self) -> Time {
        self.validity.not_after()
    }

    pub fn serial(&self) -> Serial {
        self.serial
    }

    pub fn uri(&self) -> &uri::Rsync {
        &self.uri
    }

    pub fn base64(&self) -> &Base64 {
        &self.base64
    }

    pub fn hash(&self) -> Hash {
        self.hash
    }
}
