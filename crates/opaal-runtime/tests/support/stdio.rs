#![allow(dead_code)]
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use opaal_platform::operational::{OperationalError, OperationalErrorKind};
use opaal_platform::standard_host::{
    FillError, MAX_STREAM_CHUNK_BYTES, StandardHost, StandardStream, TransferError,
};
use opaal_platform::{AuthorityProfile, Capabilities, FakePlatform};
use opaal_runtime::Value;
use opaal_runtime::authority::{
    AuthorityContext, AuthorityRule, CapabilityRequest, EffectSet, EvaluationContextId,
    RequiredEnforcement,
};
use opaal_runtime::context::OperationalContext;
use opaal_runtime::eval::{CancellationToken, FakeClock, ResourceBudget};
use opaal_runtime::operational::ModuleError;
use opaal_runtime::operational::standard::{StandardLimits, StandardState};

pub fn requests() -> [CapabilityRequest; 4] {
    [
        CapabilityRequest::entropy_system(),
        CapabilityRequest::stdin_read(),
        CapabilityRequest::stdout_write(),
        CapabilityRequest::stderr_write(),
    ]
}

pub fn authority() -> AuthorityContext {
    AuthorityContext::new(
        EvaluationContextId::new(71).unwrap(),
        requests()
            .into_iter()
            .map(|request| AuthorityRule::grant(request, RequiredEnforcement::Enforced)),
    )
    .unwrap()
}

#[derive(Default)]
pub struct Script {
    pub input: VecDeque<u8>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub calls: Vec<(StandardStream, usize)>,
    pub max_progress: Option<usize>,
    pub errors: VecDeque<TransferError>,
    pub available: [bool; 3],
    pub closed: usize,
    pub close_fail: bool,
    pub advance: Option<Arc<FakeClock>>,
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
        self.script.lock().unwrap().closed == 0
    }
    fn fill(&mut self, bytes: &mut [u8], cancelled: &dyn Fn() -> bool) -> Result<(), FillError> {
        assert!(!cancelled());
        bytes.fill(0x71);
        Ok(())
    }
    fn stream_available(&self, stream: StandardStream) -> bool {
        let script = self.script.lock().unwrap();
        script.closed == 0 && script.available[stream as usize]
    }
    fn read(
        &mut self,
        bytes: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<usize, TransferError> {
        assert!(!bytes.is_empty() && bytes.len() <= MAX_STREAM_CHUNK_BYTES);
        assert!(!cancelled());
        let mut script = self.script.lock().unwrap();
        script.calls.push((StandardStream::Stdin, bytes.len()));
        if let Some(clock) = &script.advance {
            clock.advance(30_000_000_000);
        }
        if let Some(error) = script.errors.pop_front() {
            return Err(error);
        }
        let count = bytes
            .len()
            .min(script.input.len())
            .min(script.max_progress.unwrap_or(usize::MAX));
        for byte in &mut bytes[..count] {
            *byte = script.input.pop_front().unwrap();
        }
        Ok(count)
    }
    fn write(
        &mut self,
        stream: StandardStream,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<usize, TransferError> {
        assert!(!bytes.is_empty() && bytes.len() <= MAX_STREAM_CHUNK_BYTES);
        assert!(!cancelled());
        assert_ne!(stream, StandardStream::Stdin);
        let mut script = self.script.lock().unwrap();
        script.calls.push((stream, bytes.len()));
        if let Some(clock) = &script.advance {
            clock.advance(30_000_000_000);
        }
        if let Some(error) = script.errors.pop_front() {
            return Err(error);
        }
        let count = bytes.len().min(script.max_progress.unwrap_or(usize::MAX));
        let output = if stream == StandardStream::Stdout {
            &mut script.stdout
        } else {
            &mut script.stderr
        };
        output.extend_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn close(&mut self) -> Result<(), OperationalError> {
        let mut script = self.script.lock().unwrap();
        script.closed += 1;
        if script.close_fail {
            Err(OperationalError::new(
                OperationalErrorKind::Protocol,
                "injected cleanup failure",
            ))
        } else {
            Ok(())
        }
    }
}

pub struct Harness {
    pub context: OperationalContext,
    pub effects: EffectSet,
    pub platform: FakePlatform,
    pub state: StandardState,
    pub budget: ResourceBudget,
    pub script: Arc<Mutex<Script>>,
    pub clock: Arc<FakeClock>,
}
impl Harness {
    pub fn new(input: &[u8]) -> Self {
        let script = Arc::new(Mutex::new(Script {
            input: input.iter().copied().collect(),
            available: [true; 3],
            ..Script::default()
        }));
        let clock = Arc::new(FakeClock::new());
        Self {
            context: OperationalContext::new(
                authority(),
                CancellationToken::never(),
                clock.clone(),
                None,
            ),
            effects: EffectSet::new(requests()),
            platform: FakePlatform::with_authority_profile(
                Capabilities::full(),
                AuthorityProfile::enforced(),
            ),
            state: StandardState::new(
                Some(Box::new(Host {
                    script: script.clone(),
                    evaluation: 71,
                })),
                StandardLimits::default(),
            )
            .unwrap(),
            budget: ResourceBudget::opaal(),
            script,
            clock,
        }
    }
    pub fn invoke(&mut self, name: &str, args: &[Value]) -> Result<Value, ModuleError> {
        self.state.invoke_stdio(
            &self.context,
            &self.effects,
            &self.platform,
            &mut self.budget,
            name,
            args,
        )
    }
    pub fn limits(&mut self, limits: StandardLimits) {
        self.state = StandardState::new(
            Some(Box::new(Host {
                script: self.script.clone(),
                evaluation: 71,
            })),
            limits,
        )
        .unwrap();
    }
}

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
            b"import std::io as io\nexport { io }\n".to_vec()
        } else {
            self.0.as_bytes().to_vec()
        })
    }
}
pub fn try_load(
    body: &str,
) -> Result<opaal_runtime::module::ModuleProgram, opaal_runtime::module::ModuleProgramError> {
    let source = Source(format!("import std::io as io\n{body}"));
    opaal_runtime::module::ModuleProgramLoader::new(&source, &source)
        .load(Path::new("/stdio/main.opaal"))
}
pub fn load(body: &str) -> opaal_runtime::module::ModuleProgram {
    try_load(body).unwrap_or_else(|error| panic!("{error:?}\n{body}"))
}
