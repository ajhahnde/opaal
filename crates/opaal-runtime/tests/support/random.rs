#![allow(dead_code)]
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use opaal_platform::operational::{OperationalError, OperationalErrorKind};
use opaal_platform::standard_host::{FillError, StandardHost, validate_fill};
use opaal_platform::{AuthorityProfile, Capabilities, FakePlatform};
use opaal_runtime::Value;
use opaal_runtime::authority::{
    AuthorityContext, AuthorityRule, CapabilityRequest, EffectSet, EvaluationContextId,
    RequiredEnforcement,
};
use opaal_runtime::context::OperationalContext;
use opaal_runtime::eval::{CancellationToken, FakeClock, ResourceBudget};
use opaal_runtime::operational::ModuleError;
use opaal_runtime::operational::random::{RandomLimits, RandomState};

pub struct Source(pub String);
impl opaal_runtime::module::ModuleCanonicalizer for Source {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, opaal_runtime::module::ModulePathError> {
        Ok(path.to_path_buf())
    }
}
impl opaal_runtime::module::ModuleSourceLoader for Source {
    fn load(
        &self,
        module: &opaal_runtime::module::ModuleId,
    ) -> Result<Vec<u8>, opaal_runtime::module::ModuleSourceError> {
        Ok(if module.path().file_name().unwrap() == "api.opaal" {
            b"import std::random as random\nexport { random }\n".to_vec()
        } else {
            self.0.as_bytes().to_vec()
        })
    }
}
pub fn try_load(
    body: &str,
) -> Result<opaal_runtime::module::ModuleProgram, opaal_runtime::module::ModuleProgramError> {
    let source = Source(format!("import std::random as random\n{body}"));
    opaal_runtime::module::ModuleProgramLoader::new(&source, &source)
        .load(Path::new("/random/main.opaal"))
}
pub fn load(body: &str) -> opaal_runtime::module::ModuleProgram {
    try_load(body).unwrap_or_else(|error| panic!("{error:?}\n{body}"))
}

#[derive(Default)]
pub struct Script {
    pub bytes: VecDeque<u8>,
    pub fills: Vec<usize>,
    pub fail: bool,
    pub repeat: Option<u8>,
    pub closed: usize,
    pub not_started: bool,
    pub advance: Option<Arc<FakeClock>>,
    pub cleanup_fail: bool,
    pub unavailable: bool,
    pub fail_after_confirmation: bool,
    pub error_message: Option<&'static str>,
    pub advance_nanos: Option<u64>,
}
pub struct Host {
    pub script: Arc<Mutex<Script>>,
    pub evaluation: u64,
}
impl StandardHost for Host {
    fn evaluation(&self) -> u64 {
        self.evaluation
    }
    fn available(&self) -> bool {
        let script = self.script.lock().unwrap();
        !script.unavailable && script.closed == 0
    }
    fn fill(
        &mut self,
        destination: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), FillError> {
        validate_fill(destination.len()).map_err(FillError::not_started)?;
        assert!(!cancelled());
        let mut script = self.script.lock().unwrap();
        if script.not_started {
            return Err(FillError::not_started(OperationalError::new(
                OperationalErrorKind::Unsupported,
                "injected unavailable launch",
            )));
        }
        script.fills.push(destination.len());
        if let Some(clock) = &script.advance {
            clock.advance(script.advance_nanos.unwrap_or(30_000_000_000));
        }
        if script.fail {
            destination.fill(0x42);
            let mut error = FillError::attempted(OperationalError::new(
                OperationalErrorKind::Io(std::io::ErrorKind::Other),
                script.error_message.unwrap_or("injected fill failure"),
            ));
            if script.cleanup_fail {
                error.cleanup_error = Some(OperationalError::new(
                    OperationalErrorKind::Io(std::io::ErrorKind::Other),
                    script.error_message.unwrap_or("injected cleanup failure"),
                ));
            }
            return Err(error);
        }
        for byte in destination {
            *byte = if let Some(repeat) = script.repeat {
                repeat
            } else {
                script
                    .bytes
                    .pop_front()
                    .expect("scripted entropy is sufficient")
            };
        }
        if script.fail_after_confirmation {
            return Err(FillError::confirmed(OperationalError::new(
                OperationalErrorKind::Protocol,
                "injected acknowledgement failure",
            )));
        }
        Ok(())
    }
    fn close(&mut self) -> Result<(), OperationalError> {
        let mut script = self.script.lock().unwrap();
        script.closed += 1;
        if script.cleanup_fail {
            return Err(OperationalError::new(
                OperationalErrorKind::Io(std::io::ErrorKind::Other),
                script.error_message.unwrap_or("injected close failure"),
            ));
        }
        Ok(())
    }
}

pub struct Harness {
    pub context: OperationalContext,
    pub effects: EffectSet,
    pub platform: FakePlatform,
    pub state: RandomState,
    pub budget: ResourceBudget,
    pub script: Arc<Mutex<Script>>,
    pub clock: Arc<FakeClock>,
}
impl Harness {
    pub fn new(words: &[u64], tail: &[u8]) -> Self {
        let script = Arc::new(Mutex::new(Script {
            bytes: words
                .iter()
                .flat_map(|word| word.to_be_bytes())
                .chain(tail.iter().copied())
                .collect(),
            ..Script::default()
        }));
        let clock = Arc::new(FakeClock::new());
        let request = CapabilityRequest::entropy_system();
        let evaluation = EvaluationContextId::new(71).unwrap();
        let context = OperationalContext::new(
            AuthorityContext::new(
                evaluation,
                [AuthorityRule::grant(
                    request.clone(),
                    RequiredEnforcement::Enforced,
                )],
            )
            .unwrap(),
            CancellationToken::never(),
            clock.clone(),
            None,
        );
        Self {
            context,
            effects: EffectSet::new([request]),
            platform: FakePlatform::with_authority_profile(
                Capabilities::full(),
                AuthorityProfile::enforced(),
            ),
            state: RandomState::new(
                Some(Box::new(Host {
                    script: script.clone(),
                    evaluation: 71,
                })),
                RandomLimits::default(),
            )
            .unwrap(),
            budget: ResourceBudget::opaal(),
            script,
            clock,
        }
    }
    pub fn invoke(&mut self, name: &str, arguments: &[Value]) -> Result<Value, ModuleError> {
        self.state.invoke(
            &self.context,
            &self.effects,
            &self.platform,
            &mut self.budget,
            name,
            arguments,
        )
    }
    pub fn limits(&mut self, limits: RandomLimits) {
        self.state = RandomState::new(
            Some(Box::new(Host {
                script: self.script.clone(),
                evaluation: 71,
            })),
            limits,
        )
        .unwrap();
    }
    pub fn fills(&self) -> Vec<usize> {
        self.script.lock().unwrap().fills.clone()
    }
}
