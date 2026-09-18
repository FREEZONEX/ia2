#[test]
fn unique_existing_non_ascii_channel_keeps_its_name() {
    let xml = r##"<EtherCATInfo><Descriptions>
      <Devices><Device><Type ProductCode="#x74">CPL</Type>
      <Sm StartAddress="#x1400" ControlByte="#x20">Inputs</Sm></Device></Devices>
      <Modules><Module><Type ModuleIdent="#x30">AI</Type><TxPdo><Index>#x1A02</Index>
      <Entry><Index>#x6200</Index><SubIndex>1</SubIndex><BitLen>16</BitLen>
      <Name>输入电压</Name><DataType>UINT</DataType></Entry>
      </TxPdo></Module></Modules></Descriptions></EtherCATInfo>"##;
    let image = esi::assemble(&esi::parse(xml).unwrap(), &[0x30]).unwrap();
    assert_eq!(image.channels.len(), 1);
    assert_eq!(image.channels[0].name, "m0_");
    // The same label in another slot is already distinct; neither binding
    // should change when an unchanged ESI is assembled again.
    let image = esi::assemble(&esi::parse(xml).unwrap(), &[0x30, 0x30]).unwrap();
    let names: Vec<_> = image.channels.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["m0_", "m1_"]);
}
