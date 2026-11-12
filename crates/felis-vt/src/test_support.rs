use crate::{OscEnd, Sink};

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LogSink {
    pub(crate) events: Vec<Event>,
    pub(crate) yield_on_apc: bool,
    pub(crate) yield_on_osc: bool,
    pub(crate) armed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Print(u8),
    Execute(u8),
    Esc {
        intermediates: Vec<u8>,
        final_byte: u8,
    },
    Csi {
        params: Vec<u16>,
        subparams: u32,
        intermediates: Vec<u8>,
        ignore: bool,
        final_byte: u8,
    },
    DcsHook {
        params: Vec<u16>,
        intermediates: Vec<u8>,
        ignore: bool,
        final_byte: u8,
    },
    DcsPut(u8),
    DcsUnhook,
    Osc {
        body: Vec<u8>,
        end: OscEnd,
    },
    OscOverflow,
    Apc(Vec<u8>),
    ApcOverflow,
}

impl Sink for LogSink {
    fn print(&mut self, byte: u8) {
        self.events.push(Event::Print(byte));
    }
    fn execute(&mut self, byte: u8) {
        self.events.push(Event::Execute(byte));
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], final_byte: u8) {
        self.events.push(Event::Esc {
            intermediates: intermediates.to_vec(),
            final_byte,
        });
    }
    fn csi_dispatch(
        &mut self,
        params: &[u16],
        subparams: u32,
        intermediates: &[u8],
        ignore: bool,
        final_byte: u8,
    ) {
        self.events.push(Event::Csi {
            params: params.to_vec(),
            subparams,
            intermediates: intermediates.to_vec(),
            ignore,
            final_byte,
        });
    }
    fn dcs_hook(&mut self, params: &[u16], intermediates: &[u8], ignore: bool, final_byte: u8) {
        self.events.push(Event::DcsHook {
            params: params.to_vec(),
            intermediates: intermediates.to_vec(),
            ignore,
            final_byte,
        });
    }
    fn dcs_put(&mut self, byte: u8) {
        self.events.push(Event::DcsPut(byte));
    }
    fn dcs_unhook(&mut self) {
        self.events.push(Event::DcsUnhook);
    }
    fn osc_dispatch(&mut self, body: &[u8], end: OscEnd) {
        self.events.push(Event::Osc {
            body: body.to_vec(),
            end,
        });
        self.armed = self.yield_on_osc;
    }
    fn osc_overflow(&mut self) {
        self.events.push(Event::OscOverflow);
    }
    fn apc_dispatch(&mut self, body: &[u8]) {
        self.events.push(Event::Apc(body.to_vec()));
        self.armed = self.yield_on_apc;
    }
    fn take_yield(&mut self) -> bool {
        std::mem::take(&mut self.armed)
    }
    fn apc_overflow(&mut self) {
        self.events.push(Event::ApcOverflow);
    }
}
