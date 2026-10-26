//! The order a quote is verified against.
//!
//! Nobody holds what a quoted order sells, so the simulation fabricates it.
//! The bundle endpoint checks no signature, so the creation leg lists whoever
//! can conjure the sell tokens as a signer: the mint authority mints them, or
//! a donor's lamports wrap into wSOL.

use {
    super::{Order, Trade, order_uid::OrderUid, settlement},
    crate::infra::blockchain::{
        associated_token_address,
        create_associated_token_account_idempotent,
    },
    cow_settlement_client::instruction::CreateOrder,
    cow_settlement_interface::{
        data::intent::OrderIntent,
        pda::order::find_order_pda,
        token_program::TokenProgram,
    },
    solana_sdk::{
        message::{Message, VersionedMessage},
        pubkey::Pubkey,
        signature::{Keypair, Signature},
        signer::Signer,
        transaction::VersionedTransaction,
    },
    solana_system_interface::instruction::transfer,
    spl_token_2022_interface::instruction::{approve, mint_to, sync_native},
};

/// Where a simulated order's sell tokens come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Funding {
    /// `MintTo` by the mint authority. `signers` is empty when the authority
    /// signs itself, or the threshold's worth of its multisig's signers.
    Mint {
        authority: Pubkey,
        signers: Vec<Pubkey>,
    },
    /// A system transfer of `donor`'s lamports into the wSOL account, synced
    /// into tokens.
    Lamports { donor: Pubkey },
}

/// The order `trade` fills exactly at its executed amounts, in place of the
/// placeholder `quoted` was solved as. A fresh wallet owns it and the fill
/// moves through its associated token accounts: the sell one the creation
/// leg funds, the buy one the settlement creates, or the wallet itself for a
/// native SOL buy. `buy_program` is `None` for a native SOL buy.
pub(crate) fn order(
    program_id: &Pubkey,
    quoted: &Order,
    trade: &Trade,
    sell_program: TokenProgram,
    buy_program: Option<TokenProgram>,
) -> Result<Order, settlement::Error> {
    let owner = Keypair::new().pubkey();
    let buy_token_account = match buy_program {
        Some(program) => associated_token_address(&owner, &quoted.buy_token, program),
        None => owner,
    };
    let mut order = Order {
        uid: OrderUid([0; 32]),
        owner,
        sell_token: quoted.sell_token,
        buy_token: quoted.buy_token,
        sell_token_account: associated_token_address(&owner, &quoted.sell_token, sell_program),
        buy_token_account,
        sell_amount: trade.executed_sell,
        buy_amount: trade.executed_buy,
        valid_to: u32::MAX,
        side: quoted.side,
        partially_fillable: false,
        order_pda: Pubkey::default(),
        app_data: [0; 32],
        executed: 0,
        sell_balance: None,
    };
    let uid = OrderIntent::try_from(&order)?.uid();
    order.uid = OrderUid(uid.to_bytes());
    order.order_pda = find_order_pda(program_id, &uid).0;
    Ok(order)
}

/// The transaction standing in for `order`'s creation in the bundle: it
/// makes the sell token account, fills it from `funding`, delegates the sell
/// amount to the settlement state PDA and creates the order. `payer` pays the
/// fee and the rents, so the owner needs no lamports.
pub(crate) fn creation_leg(
    program_id: &Pubkey,
    payer: &Pubkey,
    order: &Order,
    program: TokenProgram,
    funding: &Funding,
) -> Result<VersionedTransaction, settlement::Error> {
    let token_program = program.address();
    let account = order.sell_token_account;
    let amount = order.sell_amount;
    let mut instructions = vec![create_associated_token_account_idempotent(
        payer,
        &order.owner,
        &order.sell_token,
        program,
    )];
    match funding {
        Funding::Mint { authority, signers } => {
            let signers: Vec<&Pubkey> = signers.iter().collect();
            instructions.push(
                mint_to(
                    &token_program,
                    &order.sell_token,
                    &account,
                    authority,
                    &signers,
                    amount,
                )
                .expect("a token program id passes the program check"),
            );
        }
        Funding::Lamports { donor } => instructions.extend([
            transfer(donor, &account, amount),
            sync_native(&token_program, &account)
                .expect("a token program id passes the program check"),
        ]),
    }
    let state_pda = settlement::find_state_pda(program_id).0;
    instructions.push(
        approve(
            &token_program,
            &account,
            &state_pda,
            &order.owner,
            &[],
            amount,
        )
        .expect("a token program id passes the program check"),
    );
    instructions.push(
        CreateOrder {
            program_id: *program_id,
            owner: order.owner,
            created_by: *payer,
            intent: &OrderIntent::try_from(order)?,
        }
        .into(),
    );
    let message = VersionedMessage::Legacy(Message::new(&instructions, Some(payer)));
    Ok(VersionedTransaction {
        signatures: vec![Signature::default(); message.header().num_required_signatures.into()],
        message,
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::domain::Side,
        cow_settlement_interface::data::intent::ENCODED_NATIVE_SOL_TRANSFER,
        solana_sdk::hash::Hash,
        spl_token_interface::native_mint,
    };

    fn pubkey(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    fn quoted(buy_token: Pubkey) -> Order {
        Order {
            uid: OrderUid([0; 32]),
            owner: Pubkey::default(),
            sell_token: pubkey(0x33),
            buy_token,
            sell_token_account: Pubkey::default(),
            buy_token_account: Pubkey::default(),
            sell_amount: 1_000,
            buy_amount: 0,
            valid_to: u32::MAX,
            side: Side::Sell,
            partially_fillable: false,
            order_pda: Pubkey::default(),
            app_data: [0; 32],
            executed: 0,
            sell_balance: None,
        }
    }

    fn trade() -> Trade {
        Trade {
            order_uid: OrderUid([0; 32]),
            executed_sell: 1_000,
            executed_buy: 2_000,
            solver_fee: 0,
        }
    }

    /// The signers a legacy message requires, in header order.
    fn signers(transaction: &VersionedTransaction) -> &[Pubkey] {
        let keys = transaction.message.static_account_keys();
        &keys[..transaction.message.header().num_required_signatures.into()]
    }

    /// The program of each instruction, in order.
    fn programs(transaction: &VersionedTransaction) -> Vec<Pubkey> {
        let keys = transaction.message.static_account_keys();
        transaction
            .message
            .instructions()
            .iter()
            .map(|instruction| keys[usize::from(instruction.program_id_index)])
            .collect()
    }

    /// The order fills exactly the executed amounts, its uid and PDA derive
    /// from that intent, and its token accounts are the fresh owner's
    /// associated ones under each mint's program.
    #[test]
    fn the_order_fills_the_trade_exactly_through_the_owners_atas() {
        let program_id = pubkey(0xaa);
        let order = order(
            &program_id,
            &quoted(pubkey(0x44)),
            &trade(),
            TokenProgram::SplToken,
            Some(TokenProgram::Token2022),
        )
        .unwrap();
        assert_eq!((order.sell_amount, order.buy_amount), (1_000, 2_000));
        assert!(!order.partially_fillable);
        assert_eq!(
            order.sell_token_account,
            associated_token_address(&order.owner, &pubkey(0x33), TokenProgram::SplToken)
        );
        assert_eq!(
            order.buy_token_account,
            associated_token_address(&order.owner, &pubkey(0x44), TokenProgram::Token2022)
        );
        let uid = OrderIntent::try_from(&order).unwrap().uid();
        assert_eq!(Hash::new_from_array(order.uid.0), uid);
        assert_eq!(order.order_pda, find_order_pda(&program_id, &uid).0);
        // Every quote gets a wallet of its own.
        let again = order_again(&program_id);
        assert_ne!(again.owner, order.owner);
    }

    fn order_again(program_id: &Pubkey) -> Order {
        order(
            program_id,
            &quoted(pubkey(0x44)),
            &trade(),
            TokenProgram::SplToken,
            Some(TokenProgram::Token2022),
        )
        .unwrap()
    }

    #[test]
    fn a_native_sol_buy_pays_out_to_the_wallet() {
        let order = order(
            &pubkey(0xaa),
            &quoted(ENCODED_NATIVE_SOL_TRANSFER),
            &trade(),
            TokenProgram::SplToken,
            None,
        )
        .unwrap();
        assert_eq!(order.buy_token_account, order.owner);
    }

    #[test]
    fn a_zero_leg_has_no_order() {
        let trade = Trade {
            executed_buy: 0,
            ..trade()
        };
        assert!(matches!(
            order(
                &pubkey(0xaa),
                &quoted(pubkey(0x44)),
                &trade,
                TokenProgram::SplToken,
                Some(TokenProgram::SplToken),
            ),
            Err(settlement::Error::ZeroAmount(_))
        ));
    }

    /// The mint authority mints the sell amount into the owner's new sell
    /// account, the owner delegates it to the state PDA and creates the
    /// order, the payer pays: those three sign, nobody else.
    #[test]
    fn the_creation_leg_mints_delegates_and_creates() {
        let (program_id, payer, authority) = (pubkey(0xaa), pubkey(0xbb), pubkey(0xcc));
        let order = order_again(&program_id);
        let leg = creation_leg(
            &program_id,
            &payer,
            &order,
            TokenProgram::SplToken,
            &Funding::Mint {
                authority,
                signers: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(
            programs(&leg),
            vec![
                spl_associated_token_account_interface::program::ID,
                spl_token_interface::ID,
                spl_token_interface::ID,
                program_id,
            ]
        );
        let mut signers = signers(&leg).to_vec();
        assert_eq!(signers.remove(0), payer);
        signers.sort();
        let mut expected = vec![order.owner, authority];
        expected.sort();
        assert_eq!(signers, expected);
        let keys = leg.message.static_account_keys();
        let instructions = leg.message.instructions();
        let mint_to = &instructions[1];
        assert_eq!(
            mint_to
                .accounts
                .iter()
                .map(|i| keys[usize::from(*i)])
                .collect::<Vec<_>>(),
            vec![order.sell_token, order.sell_token_account, authority]
        );
        assert_eq!(&mint_to.data[1..], &1_000u64.to_le_bytes());
        let approve = &instructions[2];
        assert_eq!(
            approve
                .accounts
                .iter()
                .map(|i| keys[usize::from(*i)])
                .collect::<Vec<_>>(),
            vec![
                order.sell_token_account,
                settlement::find_state_pda(&program_id).0,
                order.owner,
            ]
        );
        assert_eq!(&approve.data[1..], &1_000u64.to_le_bytes());
        let create = &instructions[3];
        assert_eq!(
            create
                .accounts
                .iter()
                .map(|i| keys[usize::from(*i)])
                .take(3)
                .collect::<Vec<_>>(),
            vec![order.owner, payer, order.order_pda]
        );
        assert_eq!(leg.signatures.len(), 3);
    }

    /// A multisig authority's signers sign in its place.
    #[test]
    fn a_multisig_authority_lists_its_signers() {
        let (program_id, payer, authority) = (pubkey(0xaa), pubkey(0xbb), pubkey(0xcc));
        let order = order_again(&program_id);
        let leg = creation_leg(
            &program_id,
            &payer,
            &order,
            TokenProgram::Token2022,
            &Funding::Mint {
                authority,
                signers: vec![pubkey(0xd1), pubkey(0xd2)],
            },
        )
        .unwrap();
        let signers = signers(&leg);
        assert!(signers.contains(&pubkey(0xd1)) && signers.contains(&pubkey(0xd2)));
        assert!(!signers.contains(&authority));
        assert_eq!(programs(&leg)[1], spl_token_2022_interface::ID);
    }

    /// A donor's lamports fund the wSOL account, synced into tokens before the
    /// delegation.
    #[test]
    fn the_creation_leg_wraps_a_donors_lamports() {
        let (program_id, payer, donor) = (pubkey(0xaa), pubkey(0xbb), pubkey(0xdd));
        let mut quoted = quoted(pubkey(0x44));
        quoted.sell_token = native_mint::ID;
        let order = order(
            &program_id,
            &quoted,
            &trade(),
            TokenProgram::SplToken,
            Some(TokenProgram::SplToken),
        )
        .unwrap();
        let leg = creation_leg(
            &program_id,
            &payer,
            &order,
            TokenProgram::SplToken,
            &Funding::Lamports { donor },
        )
        .unwrap();
        assert_eq!(
            programs(&leg),
            vec![
                spl_associated_token_account_interface::program::ID,
                solana_system_interface::program::ID,
                spl_token_interface::ID,
                spl_token_interface::ID,
                program_id,
            ]
        );
        assert!(signers(&leg).contains(&donor));
        let keys = leg.message.static_account_keys();
        let transfer = &leg.message.instructions()[1];
        assert_eq!(
            transfer
                .accounts
                .iter()
                .map(|i| keys[usize::from(*i)])
                .collect::<Vec<_>>(),
            vec![donor, order.sell_token_account]
        );
        assert_eq!(&transfer.data[4..], &1_000u64.to_le_bytes());
    }
}
