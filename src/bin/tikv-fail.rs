// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

#![feature(plugin)]
#![cfg_attr(feature = "dev", plugin(clippy))]
#![cfg_attr(not(feature = "dev"), allow(unknown_lints))]
#![allow(needless_pass_by_value)]

extern crate tikv;
extern crate clap;
extern crate grpcio as grpc;
extern crate protobuf;
extern crate kvproto;

use std::str;
use std::time::Duration;
use std::sync::Arc;

use clap::{App, Arg};
use grpc::{CallOption, ChannelBuilder, EnvBuilder};
use protobuf::RepeatedField;
use kvproto::tikvpb;
use kvproto::tikvpb_grpc::TikvClient;

fn main() {
    let app = App::new("TiKV fail point")
        .author("PingCAP")
        .about(
            "Distributed transactional key value database powered by Rust and Raft",
        )
        .arg(
            Arg::with_name("addr")
                .short("a")
                .takes_value(true)
                .help("set tikv ip:port"),
        );
    let matches = app.clone().get_matches();
    let addr = matches.value_of("addr").unwrap();
    let addr = addr.trim_left_matches("http://");

    let env = Arc::new(EnvBuilder::new().name_prefix("tikv-fail").build());
    let channel = ChannelBuilder::new(env).connect(addr);
    let client = TikvClient::new(channel);

    let mut fail_cfgs = vec![];
    let mut fail_cfg = tikvpb::FailPointCfg::new();
    fail_cfg.set_name(
        "tikv::raftstore::store::store::raft_between_save".to_owned(),
    );
    fail_cfg.set_actions("panic(fail_point_raft_between_write)".to_owned());
    fail_cfgs.push(fail_cfg);

    let mut req = tikvpb::FailPointRequest::new();
    req.set_fail_cfgs(RepeatedField::from_vec(fail_cfgs));
    println!("req {:?}", req);

    let option = CallOption::default().timeout(Duration::from_secs(10));
    let resp = client.fail_point_opt(req.clone(), option).unwrap();
    println!("resp {:?}", resp);
}