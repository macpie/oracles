use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use file_store::iot_packet::PacketRouterPacketReport;
use futures_util::stream;
use helium_crypto::PublicKeyBinary;
use helium_proto::{
    services::{
        packet_verifier::{InvalidPacket, InvalidPacketReason, ValidPacket},
        router::packet_router_packet_report_v1::PacketType,
    },
    DataRate, Region,
};
use iot_packet_verifier::{
    balances::{BalanceCache, PayerAccount},
    burner::Burner,
    pending::{confirm_pending_txns, AddPendingBurn, Burn, MockPendingTables, PendingTables},
    verifier::{payload_size_to_dc, ConfigServer, ConfigServerError, Org, Verifier, BYTES_PER_DC},
};
use solana::{
    burn::{MockTransaction, SolanaNetwork},
    GetSignature, SolanaRpcError,
};
use solana_sdk::signature::Signature;
use sqlx::PgPool;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

struct MockConfig {
    payer: PublicKeyBinary,
    enabled: bool,
}

#[derive(Default, Clone)]
struct MockConfigServer {
    payers: Arc<Mutex<HashMap<u64, MockConfig>>>,
}

impl MockConfigServer {
    async fn insert(&self, oui: u64, payer: PublicKeyBinary) {
        self.payers.lock().await.insert(
            oui,
            MockConfig {
                payer,
                enabled: true,
            },
        );
    }
}

#[async_trait]
impl ConfigServer for MockConfigServer {
    async fn fetch_org(
        &self,
        oui: u64,
        _cache: &mut HashMap<u64, PublicKeyBinary>,
    ) -> Result<PublicKeyBinary, ConfigServerError> {
        Ok(self.payers.lock().await.get(&oui).unwrap().payer.clone())
    }

    async fn disable_org(&self, oui: u64) -> Result<(), ConfigServerError> {
        self.payers.lock().await.get_mut(&oui).unwrap().enabled = false;
        Ok(())
    }

    async fn enable_org(&self, oui: u64) -> Result<(), ConfigServerError> {
        self.payers.lock().await.get_mut(&oui).unwrap().enabled = true;
        Ok(())
    }

    async fn list_orgs(&self) -> Result<Vec<Org>, ConfigServerError> {
        Ok(self
            .payers
            .lock()
            .await
            .iter()
            .map(|(oui, config)| Org {
                oui: *oui,
                payer: config.payer.clone(),
                locked: !config.enabled,
            })
            .collect())
    }
}

fn packet_report(
    oui: u64,
    timestamp: u64,
    payload_size: u32,
    payload_hash: Vec<u8>,
    free: bool,
) -> PacketRouterPacketReport {
    PacketRouterPacketReport {
        received_timestamp: Utc.timestamp_opt(timestamp as i64, 0).unwrap(),
        oui,
        net_id: 0,
        rssi: 0,
        free,
        frequency: 0,
        snr: 0.0,
        data_rate: DataRate::Fsk50,
        region: Region::As9231,
        gateway: PublicKeyBinary::from(vec![]),
        payload_hash,
        payload_size,
        packet_type: PacketType::Uplink,
    }
}

fn join_packet_report(
    oui: u64,
    timestamp: u64,
    payload_size: u32,
    payload_hash: Vec<u8>,
) -> PacketRouterPacketReport {
    PacketRouterPacketReport {
        received_timestamp: Utc.timestamp_opt(timestamp as i64, 0).unwrap(),
        oui,
        net_id: 0,
        rssi: 0,
        free: true,
        frequency: 0,
        snr: 0.0,
        data_rate: DataRate::Fsk50,
        region: Region::As9231,
        gateway: PublicKeyBinary::from(vec![]),
        payload_hash,
        payload_size,
        packet_type: PacketType::Join,
    }
}

fn valid_packet(
    timestamp: u64,
    payload_size: u32,
    payload_hash: Vec<u8>,
    paid: bool,
) -> ValidPacket {
    ValidPacket {
        payload_size,
        payload_hash,
        gateway: vec![],
        num_dcs: if paid {
            payload_size_to_dc(payload_size as u64) as u32
        } else {
            0
        },
        packet_timestamp: timestamp,
    }
}

fn invalid_packet(payload_size: u32, payload_hash: Vec<u8>) -> InvalidPacket {
    InvalidPacket {
        payload_size,
        payload_hash,
        gateway: vec![],
        reason: InvalidPacketReason::InsufficientBalance as i32,
    }
}

#[derive(Clone)]
struct InstantlyBurnedBalance(Arc<Mutex<HashMap<PublicKeyBinary, u64>>>);

#[async_trait]
impl AddPendingBurn for InstantlyBurnedBalance {
    async fn add_burned_amount(
        &mut self,
        payer: &PublicKeyBinary,
        amount: u64,
    ) -> Result<(), sqlx::Error> {
        let mut map = self.0.lock().await;
        let balance = map.get_mut(payer).unwrap();
        *balance -= amount;
        Ok(())
    }
}

#[tokio::test]
async fn test_config_unlocking() {
    // Set up orgs:
    let orgs = MockConfigServer::default();
    orgs.insert(0_u64, PublicKeyBinary::from(vec![0])).await;
    // Set up balances:
    let mut solana_network = HashMap::new();
    solana_network.insert(PublicKeyBinary::from(vec![0]), 3);
    let solana_network = Arc::new(Mutex::new(solana_network));
    // Set up cache:
    let mut cache = HashMap::new();
    cache.insert(PublicKeyBinary::from(vec![0]), 3);
    let cache = Arc::new(Mutex::new(cache));
    let balances = InstantlyBurnedBalance(cache.clone());
    // Set up verifier:
    let mut verifier = Verifier {
        debiter: balances.0.clone(),
        config_server: orgs.clone(),
    };
    let mut valid_packets = Vec::new();
    let mut invalid_packets = Vec::new();
    verifier
        .verify(
            1,
            &mut balances.clone(),
            stream::iter(vec![
                packet_report(0, 0, 24, vec![1], false),
                packet_report(0, 1, 48, vec![2], false),
                packet_report(0, 2, 1, vec![3], false),
            ]),
            &mut valid_packets,
            &mut invalid_packets,
        )
        .await
        .unwrap();

    assert!(!orgs.payers.lock().await.get(&0).unwrap().enabled);

    // Update the solana network:
    *solana_network
        .lock()
        .await
        .get_mut(&PublicKeyBinary::from(vec![0]))
        .unwrap() = 50;

    let (trigger, listener) = triggered::trigger();

    // Calling monitor funds should re-enable the org and update the
    // verifier's cache
    let solana = solana_network.clone();
    let balance_cache = cache.clone();
    let orgs_clone = orgs.clone();
    tokio::spawn(async move {
        orgs_clone
            .monitor_funds(
                solana.clone(),
                balance_cache,
                1,
                Duration::from_secs(100),
                listener,
            )
            .await
    });

    tokio::time::sleep(Duration::from_secs(1)).await;

    // We should be re-enabled
    assert!(orgs.payers.lock().await.get(&0).unwrap().enabled);
    assert_eq!(
        *cache
            .lock()
            .await
            .get(&PublicKeyBinary::from(vec![0]))
            .unwrap(),
        50
    );

    trigger.trigger();

    verifier
        .verify(
            1,
            &mut balances.clone(),
            stream::iter(vec![
                packet_report(0, 0, 24, vec![1], false),
                packet_report(0, 1, 48, vec![2], false),
                packet_report(0, 2, 1, vec![3], false),
            ]),
            &mut valid_packets,
            &mut invalid_packets,
        )
        .await
        .unwrap();

    // Still enabled:
    assert!(orgs.payers.lock().await.get(&0).unwrap().enabled);
    assert_eq!(
        *cache
            .lock()
            .await
            .get(&PublicKeyBinary::from(vec![0]))
            .unwrap(),
        46
    );
}

#[tokio::test]
async fn test_verifier_free_packets() {
    // Org packets
    let packets = vec![
        packet_report(0, 0, 24, vec![4], true),
        packet_report(0, 1, 48, vec![5], true),
        packet_report(0, 2, 1, vec![6], true),
    ];

    let org_pubkey = PublicKeyBinary::from(vec![0]);

    // Set up orgs:
    let orgs = MockConfigServer::default();
    orgs.insert(0_u64, org_pubkey.clone()).await;

    // Set up balances:
    let mut balances = HashMap::new();
    balances.insert(org_pubkey.clone(), 5);
    let balances = InstantlyBurnedBalance(Arc::new(Mutex::new(balances)));

    // Set up output:
    let mut valid_packets = Vec::new();
    let mut invalid_packets = Vec::new();

    // Set up verifier:
    let mut verifier = Verifier {
        debiter: balances.0.clone(),
        config_server: orgs,
    };
    // Run the verifier:
    verifier
        .verify(
            1,
            &mut balances.clone(),
            stream::iter(packets),
            &mut valid_packets,
            &mut invalid_packets,
        )
        .await
        .unwrap();

    // Verify packet reports:
    assert_eq!(
        valid_packets,
        vec![
            valid_packet(0, 24, vec![4], false),
            valid_packet(1000, 48, vec![5], false),
            valid_packet(2000, 1, vec![6], false),
        ]
    );

    assert!(invalid_packets.is_empty());

    let payers = verifier.config_server.payers.lock().await;
    assert!(payers.get(&0).unwrap().enabled);

    assert_eq!(
        verifier
            .debiter
            .payer_balance(&org_pubkey)
            .await
            .expect("unchanged balance"),
        5
    );
}

#[tokio::test]
async fn test_verifier() {
    let packets = vec![
        // Packets for first OUI
        packet_report(0, 0, 24, vec![1], false),
        packet_report(0, 1, 48, vec![2], false),
        packet_report(0, 2, 1, vec![3], false),
        join_packet_report(0, 3, 1, vec![4]),
        // Packets for second OUI
        packet_report(1, 0, 24, vec![4], false),
        packet_report(1, 1, 48, vec![5], false),
        packet_report(1, 2, 1, vec![6], false),
        join_packet_report(1, 3, 1, vec![4]),
        // Packets for third OUI
        packet_report(2, 0, 24, vec![7], false),
        join_packet_report(2, 1, 1, vec![4]),
    ];
    // Set up orgs:
    let orgs = MockConfigServer::default();
    orgs.insert(0_u64, PublicKeyBinary::from(vec![0])).await;
    orgs.insert(1_u64, PublicKeyBinary::from(vec![1])).await;
    orgs.insert(2_u64, PublicKeyBinary::from(vec![2])).await;
    // Set up balances:
    let mut balances = HashMap::new();
    balances.insert(PublicKeyBinary::from(vec![0]), 3);
    balances.insert(PublicKeyBinary::from(vec![1]), 5);
    balances.insert(PublicKeyBinary::from(vec![2]), 2);
    let balances = InstantlyBurnedBalance(Arc::new(Mutex::new(balances)));
    // Set up output:
    let mut valid_packets = Vec::new();
    let mut invalid_packets = Vec::new();
    // Set up verifier:
    let mut verifier = Verifier {
        debiter: balances.0.clone(),
        config_server: orgs,
    };

    // Run the verifier:
    verifier
        .verify(
            1,
            &mut balances.clone(),
            stream::iter(packets),
            &mut valid_packets,
            &mut invalid_packets,
        )
        .await
        .unwrap();

    // Verify packet reports:
    assert_eq!(
        valid_packets,
        vec![
            // First two packets for OUI #0 are valid
            valid_packet(0, 24, vec![1], true),
            valid_packet(1000, 48, vec![2], true),
            // All packets for OUI #1 are valid
            valid_packet(0, 24, vec![4], true),
            valid_packet(1000, 48, vec![5], true),
            valid_packet(2000, 1, vec![6], true),
            // All packets for OUI #2 are valid
            valid_packet(0, 24, vec![7], true),
        ]
    );

    assert_eq!(invalid_packets, vec![invalid_packet(1, vec![3]),]);

    // Verify that only org #0 is disabled:
    let payers = verifier.config_server.payers.lock().await;
    assert!(!payers.get(&0).unwrap().enabled);
    assert!(payers.get(&1).unwrap().enabled);
    assert!(payers.get(&2).unwrap().enabled);
}

#[tokio::test]
async fn test_end_to_end() {
    let payer = PublicKeyBinary::from(vec![0]);

    // Pending tables:
    let pending_burns: Arc<Mutex<HashMap<PublicKeyBinary, u64>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let pending_tables = MockPendingTables {
        pending_txns: Default::default(),
        pending_burns: pending_burns.clone(),
    };

    // Solana network:
    let mut solana_network = HashMap::new();
    solana_network.insert(payer.clone(), 3_u64); // Start with 3 data credits
    let solana_network = Arc::new(Mutex::new(solana_network));

    // Balance cache:
    let balance_cache = BalanceCache::new(&pending_tables, solana_network.clone())
        .await
        .unwrap();

    // Burner:
    let mut burner = Burner::new(
        pending_tables.clone(),
        &balance_cache,
        Duration::default(), // Burn period does not matter, we manually burn
        solana_network.clone(),
    );

    // Orgs:
    let orgs = MockConfigServer::default();
    orgs.insert(0_u64, payer.clone()).await;

    // Packet output:
    let mut valid_packets = Vec::new();
    let mut invalid_packets = Vec::new();

    // Set up verifier:
    let mut verifier = Verifier {
        debiter: balance_cache,
        config_server: orgs,
    };

    // Verify four packets, each costing one DC. The last one should be invalid
    verifier
        .verify(
            1,
            &mut pending_burns.clone(),
            stream::iter(vec![
                packet_report(0, 0, BYTES_PER_DC as u32, vec![1], false),
                packet_report(0, 1, BYTES_PER_DC as u32, vec![2], false),
                packet_report(0, 2, BYTES_PER_DC as u32, vec![3], false),
                packet_report(0, 3, BYTES_PER_DC as u32, vec![4], false),
            ]),
            &mut valid_packets,
            &mut invalid_packets,
        )
        .await
        .unwrap();

    // Org 0 should be disabled now:
    assert!(
        !verifier
            .config_server
            .payers
            .lock()
            .await
            .get(&0)
            .unwrap()
            .enabled
    );

    assert_eq!(
        valid_packets,
        vec![
            valid_packet(0, BYTES_PER_DC as u32, vec![1], true),
            valid_packet(1000, BYTES_PER_DC as u32, vec![2], true),
            valid_packet(2000, BYTES_PER_DC as u32, vec![3], true),
        ]
    );

    // Last packet is invalid:
    assert_eq!(
        invalid_packets,
        vec![invalid_packet(BYTES_PER_DC as u32, vec![4])]
    );

    // Check current balance:
    let balance = {
        let balances = verifier.debiter.balances();
        let balances = balances.lock().await;
        *balances.get(&payer).unwrap()
    };
    assert_eq!(balance.balance, 3);
    assert_eq!(balance.burned, 3);

    // Check that 3 DC are pending to be burned:
    let pending_burn = *pending_burns.lock().await.get(&payer).unwrap();
    assert_eq!(pending_burn, 3);

    // Initiate the burn:
    burner.burn().await.unwrap();

    // Now that we've burn, the balances and burn amount should be reset:
    let balance = {
        let balances = verifier.debiter.balances();
        let balances = balances.lock().await;
        *balances.get(&payer).unwrap()
    };
    assert_eq!(balance.balance, 0);
    assert_eq!(balance.burned, 0);

    // Pending burns should be empty as well:
    let pending_burn = *pending_burns.lock().await.get(&payer).unwrap();
    assert_eq!(pending_burn, 0);

    // Additionally, the balance on the solana network should be zero:
    let solana_balance = *solana_network.lock().await.get(&payer).unwrap();
    assert_eq!(solana_balance, 0);

    // Attempting to validate one packet should fail now:
    valid_packets.clear();
    invalid_packets.clear();

    verifier
        .verify(
            1,
            &mut pending_burns.clone(),
            stream::iter(vec![packet_report(
                0,
                4,
                BYTES_PER_DC as u32,
                vec![5],
                false,
            )]),
            &mut valid_packets,
            &mut invalid_packets,
        )
        .await
        .unwrap();

    assert_eq!(valid_packets, vec![]);

    assert_eq!(
        invalid_packets,
        vec![invalid_packet(BYTES_PER_DC as u32, vec![5])]
    );
}

#[derive(Clone)]
struct MockSolanaNetwork {
    confirmed: Arc<Mutex<HashSet<Signature>>>,
    ledger: Arc<Mutex<HashMap<PublicKeyBinary, u64>>>,
}

impl MockSolanaNetwork {
    fn new(ledger: HashMap<PublicKeyBinary, u64>) -> Self {
        Self {
            confirmed: Arc::new(Default::default()),
            ledger: Arc::new(Mutex::new(ledger)),
        }
    }
}

#[async_trait]
impl SolanaNetwork for MockSolanaNetwork {
    type Transaction = MockTransaction;

    async fn payer_balance(&self, payer: &PublicKeyBinary) -> Result<u64, SolanaRpcError> {
        self.ledger.payer_balance(payer).await
    }

    async fn make_burn_transaction(
        &self,
        payer: &PublicKeyBinary,
        amount: u64,
    ) -> Result<MockTransaction, SolanaRpcError> {
        self.ledger.make_burn_transaction(payer, amount).await
    }

    async fn submit_transaction(&self, txn: &MockTransaction) -> Result<(), SolanaRpcError> {
        self.confirmed.lock().await.insert(txn.signature);
        self.ledger.submit_transaction(txn).await
    }

    async fn confirm_transaction(&self, txn: &Signature) -> Result<bool, SolanaRpcError> {
        Ok(self.confirmed.lock().await.contains(txn))
    }
}

#[sqlx::test]
#[ignore]
async fn test_pending_txns(pool: PgPool) -> anyhow::Result<()> {
    const CONFIRMED_BURN_AMOUNT: u64 = 7;
    const UNCONFIRMED_BURN_AMOUNT: u64 = 11;
    let payer: PublicKeyBinary = "112NqN2WWMwtK29PMzRby62fDydBJfsCLkCAf392stdok48ovNT6"
        .parse()
        .unwrap();
    let mut ledger = HashMap::new();
    ledger.insert(
        payer.clone(),
        CONFIRMED_BURN_AMOUNT + UNCONFIRMED_BURN_AMOUNT,
    );
    let mut cache = HashMap::new();
    cache.insert(
        payer.clone(),
        PayerAccount {
            balance: CONFIRMED_BURN_AMOUNT + UNCONFIRMED_BURN_AMOUNT,
            burned: CONFIRMED_BURN_AMOUNT + UNCONFIRMED_BURN_AMOUNT,
        },
    );
    let mock_network = MockSolanaNetwork::new(ledger);

    // Add both the burn amounts to the pending burns table
    {
        let mut transaction = pool.begin().await.unwrap();
        transaction
            .add_burned_amount(&payer, CONFIRMED_BURN_AMOUNT + UNCONFIRMED_BURN_AMOUNT)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
    }

    // First transaction is confirmed
    {
        let txn = mock_network
            .make_burn_transaction(&payer, CONFIRMED_BURN_AMOUNT)
            .await
            .unwrap();
        pool.add_pending_transaction(&payer, CONFIRMED_BURN_AMOUNT, txn.get_signature())
            .await
            .unwrap();
        mock_network.submit_transaction(&txn).await.unwrap();
    }

    // Second is unconfirmed
    {
        let txn = mock_network
            .make_burn_transaction(&payer, UNCONFIRMED_BURN_AMOUNT)
            .await
            .unwrap();
        pool.add_pending_transaction(&payer, UNCONFIRMED_BURN_AMOUNT, txn.get_signature())
            .await
            .unwrap();
    }

    // Confirm pending transactions
    confirm_pending_txns(&pool, &mock_network, &Arc::new(Mutex::new(cache)))
        .await
        .unwrap();

    let pending_burn: Burn = sqlx::query_as("SELECT * FROM pending_burns LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();

    // The unconfirmed burn amount should be what's left
    assert_eq!(pending_burn.amount, UNCONFIRMED_BURN_AMOUNT);

    Ok(())
}
