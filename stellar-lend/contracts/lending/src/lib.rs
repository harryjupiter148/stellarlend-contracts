#![no_std]

pub mod debt;
pub mod rounding_strategy;
pub mod debt;

use soroban_sdk::{contract, contractimpl, contracttype, contracterror, Address, Env, Symbol, Bytes, IntoVal, Vec, Val, vec};
use crate::debt::{DebtPosition, load_debt, repay_amount, save_debt, effective_debt};

use crate::debt::*;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Bytes, Env, IntoVal, Symbol,
};

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]  // Add Eq here
pub struct PositionSummary {
    pub collateral: i128,
    pub debt: i128,
    pub health_factor: i128,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    BelowMinimumBorrow = 1008,
    PositionHealthy = 1009,
}

#[contract]
pub struct LendingContract;

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]  // Add Eq here
pub enum EmergencyState {
    Normal,
    Shutdown,
    Recovery,
}

impl EmergencyState {
    fn as_u32(&self) -> u32 {
        match self {
            EmergencyState::Normal => 0,
            EmergencyState::Shutdown => 1,
            EmergencyState::Recovery => 2,
        }
    }
}

#[contractimpl]
impl LendingContract {
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&"admin") {
            panic!("AlreadyInitialized");
        }
        env.storage().instance().set(&"admin", &admin);
        // initialize emergency state to Normal
        env.storage().instance().set(
            &Symbol::new(&env, "EmergencyState"),
            &EmergencyState::Normal,
        );
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&"admin").unwrap()
    }

    /// Propose a new admin (current admin only)
    pub fn propose_admin(env: Env, new_admin: Address) {
        let current_admin = Self::get_admin(env.clone());
        current_admin.require_auth();
        env.storage().instance().set(&"pending_admin", &new_admin);
    }

    /// Accept the proposed admin role (proposed admin only)
    pub fn accept_admin(env: Env) {
        let pending_admin: Address = env.storage().instance().get(&"pending_admin").expect("no pending admin");
        pending_admin.require_auth();
        env.storage().instance().set(&"admin", &pending_admin);
        env.storage().instance().remove(&"pending_admin");
    }

    /// Set the minimum borrow amount (admin-only).
    pub fn set_min_borrow(env: Env, min_borrow: i128) {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "BorrowMinAmount"), &min_borrow);
    }

    /// Get the minimum borrow amount.
    pub fn get_min_borrow(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "BorrowMinAmount"))
            .unwrap_or(0)
    }

    /// Deposit collateral for a user.
    pub fn deposit(env: Env, user: Address, amount: i128) -> i128 {
        // Guard: only allowed in Normal state
        let state: EmergencyState = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "EmergencyState"))
            .unwrap_or(EmergencyState::Normal);
        if state != EmergencyState::Normal {
            panic!("DepositNotAllowedInCurrentState");
        }
        // Prevent mutating during an active flash loan callback
        let active: bool = env
            .storage()
            .instance()
            .get(&"flash_active")
            .unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let key = ("col", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current.checked_add(amount).expect("collateral overflow");
        env.storage().persistent().set(&key, &new_balance);
        Ok(new_balance)
    }

    pub fn withdraw(env: Env, user: Address, amount: i128) -> i128 {
        // Guard: not allowed in Shutdown; allowed in Normal or Recovery
        let state: EmergencyState = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "EmergencyState"))
            .unwrap_or(EmergencyState::Normal);
        if state == EmergencyState::Shutdown {
            panic!("WithdrawDisabledDuringShutdown");
        }
        // Prevent mutating during an active flash loan callback
        let active: bool = env
            .storage()
            .instance()
            .get(&"flash_active")
            .unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let key = ("col", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if amount > current {
            panic!("insufficient collateral");
        }
        let new_balance = current.checked_sub(amount).expect("collateral underflow");
        env.storage().persistent().set(&key, &new_balance);
        Ok(new_balance)
    }

    /// Borrow against deposited collateral.
    pub fn borrow(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        // Guard: only allowed in Normal state
        let state: EmergencyState = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "EmergencyState"))
            .unwrap_or(EmergencyState::Normal);
        if state != EmergencyState::Normal {
            panic!("BorrowNotAllowedInCurrentState");
        }
        user.require_auth();
        let min_borrow = Self::get_min_borrow(env.clone());
        if amount < min_borrow {
            panic!("BelowMinimumBorrow");
        }
        let key = ("debt", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_debt = current + amount;
        env.storage().persistent().set(&key, &new_debt);
        new_debt
    }

    /// Liquidate an undercollateralized position.
    pub fn liquidate(
        env: Env,
        liquidator: Address,
        borrower: Address,
        amount: i128,
    ) -> Result<i128, Error> {
        liquidator.require_auth();

        let col_key = ("col", borrower.clone());
        let debt_key = ("debt", borrower.clone());

        let collateral: i128 = env.storage().persistent().get(&col_key).unwrap_or(0);
        let debt: i128 = env.storage().persistent().get(&debt_key).unwrap_or(0);

        if debt == 0 {
            return Err(Error::PositionHealthy);
        }

        // Health Factor Calculation (base 10000). HF = (Collateral * Threshold) / Debt
        // We use a hardcoded 80% (8000 BPS) liquidation threshold for this implementation.
        const LIQUIDATION_THRESHOLD: i128 = 8000;
        let hf = (collateral * LIQUIDATION_THRESHOLD) / debt;

        if hf >= 10000 {
            return Err(Error::PositionHealthy);
        }

        // Cap maximum allowed repayment by close factor (50%)
        const CLOSE_FACTOR: i128 = 5000;
        let max_repay = (debt * CLOSE_FACTOR) / 10000;
        let actual_repay = if amount > max_repay { max_repay } else { amount };

        // Apply liquidation incentive bonus (10%)
        const INCENTIVE_BPS: i128 = 1000;
        let seized_collateral = (actual_repay * (10000 + INCENTIVE_BPS)) / 10000;
        
        // Ensure we don't seize more than available
        let final_seized = if seized_collateral > collateral { collateral } else { seized_collateral };

        let new_debt = debt - actual_repay;
        let new_col = collateral - final_seized;

        env.storage().persistent().set(&debt_key, &new_debt);
        env.storage().persistent().set(&col_key, &new_col);

        Ok(actual_repay)
    }

    pub fn repay(env: Env, user: Address, amount: i128) -> i128 {
        // Guard: not allowed in Shutdown; allowed in Normal or Recovery
        let state: EmergencyState = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "EmergencyState"))
            .unwrap_or(EmergencyState::Normal);
        if state == EmergencyState::Shutdown {
            panic!("RepayDisabledDuringShutdown");
        }
        // Prevent mutating during an active flash loan callback
        let active: bool = env
            .storage()
            .instance()
            .get(&"flash_active")
            .unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let updated = repay_amount(position, now, amount, DEFAULT_APR_BPS)
            .unwrap_or_else(|_| panic!("repay failed"));
        save_debt(&env, &user, &updated);
        Ok(updated.principal)
    }

    pub fn get_debt_position(env: Env, user: Address) -> DebtPosition {
        load_debt(&env, &user)
    }

    pub fn set_flash_loan_fee_bps(env: Env, admin: Address, fee_bps: i128) {
        admin.require_auth();
        let stored_admin: Address = env.storage().instance().get(&"admin").unwrap();
        if stored_admin != admin {
            panic!("Unauthorized");
        }
        const MAX_FEE: i128 = 1000;
        if !(0..=MAX_FEE).contains(&fee_bps) {
            panic!("InvalidFeeBps");
        }
        env.storage().instance().set(&"flash_fee_bps", &fee_bps);
    }

    /// Privileged function to update the global emergency state. Only callable by `admin` or `guardian`.
    pub fn set_emergency_state(env: Env, caller: Address, new_state: EmergencyState) {
        caller.require_auth();
        let stored_admin: Address = env.storage().instance().get(&"admin").unwrap();
        // optional guardian may be present
        let guardian_key = Symbol::new(&env, "guardian");
        let guardian: Option<Address> = env.storage().instance().get(&guardian_key).unwrap_or(None);
        // ensure caller matches admin or guardian
        let allowed = if caller == stored_admin {
            true
        } else if let Some(g) = guardian {
            g == caller
        } else {
            false
        };
        if !allowed {
            panic!("Unauthorized");
        }

        let old_state: EmergencyState = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "EmergencyState"))
            .unwrap_or(EmergencyState::Normal);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "EmergencyState"), &new_state);

        // emit event with (old_state, new_state)
        env.events().publish(
            (&Symbol::new(&env, "EmergencyStateChanged"),),
            (&old_state, &new_state),
        );
    }

    fn get_flash_fee_bps(env: &Env) -> i128 {
        env.storage().instance().get(&"flash_fee_bps").unwrap_or(5)
    }

    // Repay function used by receiver during callback to return funds to the contract.
    pub fn repay_flash_loan(env: Env, payer: Address, asset: Address, amount: i128) {
        // Payer must authorize the repayment
        payer.require_auth();
        // subtract from payer balance
        let payer_key = ("bal", asset.clone(), payer.clone());
        let payer_bal: i128 = env.storage().persistent().get(&payer_key).unwrap_or(0);
        if payer_bal < amount {
            panic!("InsufficientBalance");
        }
        env.storage()
            .persistent()
            .set(&payer_key, &(payer_bal - amount));
        // add to contract treasury
        let tre_key = ("treasury", asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        env.storage()
            .persistent()
            .set(&tre_key, &(tre_bal + amount));
    }

    pub fn flash_loan(
        env: Env,
        initiator: Address,
        receiver: Address,
        initiator: Address,
        asset: Address,
        amount: i128,
        params: Bytes,
    ) {
        let tre_key = ("treasury", asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        if amount > tre_bal {
            panic!("InsufficientLiquidity");
        }

        initiator.require_auth();
        receiver.require_auth();

        let fee_bps = Self::get_flash_fee_bps(&env);
        let fee = amount * fee_bps / 10_000;

        // transfer out: treasury -= amount; receiver balance += amount
        env.storage()
            .persistent()
            .set(&tre_key, &(tre_bal - amount));
        let rec_key = ("bal", asset.clone(), receiver.clone());
        let rec_bal: i128 = env.storage().persistent().get(&rec_key).unwrap_or(0);
        env.storage()
            .persistent()
            .set(&rec_key, &(rec_bal + amount));

        // set reentrancy guard
        env.storage().instance().set(&"flash_active", &true);

        let method = Symbol::new(&env, "on_flash_loan");
        let args = (initiator.clone(), asset.clone(), amount, fee, params).into_val(&env);
        // Call contract - if it panics, propagate
        env.invoke_contract::<()>(&receiver, &method, args);

        env.storage().instance().set(&"flash_active", &false);

        let final_tre: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        if final_tre < tre_bal + fee {
            panic!("InsufficientRepayment");
        }
    }

    pub fn get_position(env: Env, user: Address) -> PositionSummary {
        let col: i128 = env
            .storage()
            .persistent()
            .get(&("col", user.clone()))
            .unwrap_or(0);
        let position = load_debt(&env, &user);
        let debt = effective_debt(&position, env.ledger().timestamp(), DEFAULT_APR_BPS)
            .unwrap_or(position.principal);
        
        let health_factor = if debt > 0 {
            (col * 8000) / debt
        } else {
            1000000 // Sentinel for healthy
        };

        PositionSummary {
            collateral: col,
            debt,
            health_factor,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    fn setup() -> (Env, LendingContractClient<'static>, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        client.initialize(&admin);
        (env, client, admin, user)
    }

    #[test]
    fn test_initialize_and_get_admin() {
        let (_env, client, admin, _user) = setup();
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn test_propose_and_accept_admin() {
        let (env, client, admin, _user) = setup();
        let new_admin = Address::generate(&env);
        
        client.propose_admin(&new_admin);
        client.accept_admin();
        
        assert_eq!(client.get_admin(), new_admin);
    }

    #[test]
    #[should_panic(expected = "no pending admin")]
    fn test_accept_without_propose() {
        let (_env, client, _admin, _user) = setup();
        client.accept_admin();
    }

    #[test]
    fn test_deposit_increases_balance() {
        let (_env, client, _admin, user) = setup();
        assert_eq!(client.deposit(&user, &100), 100);
        assert_eq!(client.deposit(&user, &50), 150);
    }

    #[test]
    fn test_withdraw_decreases_balance() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100);
        assert_eq!(client.withdraw(&user, &40), 60);
    }

    #[test]
    fn test_repay_decreases_debt() {
        let (_env, client, _admin, user) = setup();
        // Deposit enough collateral first (150 % of 100 = 150).
        client.deposit(&user, &150);
        client.borrow(&user, &100).unwrap();
        assert_eq!(client.repay(&user, &30), 70);
    }

    #[test]
    fn test_position_summary_reflects_state() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &300);
        client.borrow(&user, &100).unwrap(); // 300/100 = 300 % ≥ 150 %
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 200);
        assert_eq!(pos.debt, 75);
        assert!(pos.health_factor > 10000);
    }

    #[test]
    fn test_liquidate_fails_if_healthy() {
        let (env, client, _admin, user) = setup();
        let liquidator = Address::generate(&env);
        client.deposit(&user, &200);
        client.borrow(&user, &100);
        let res = client.try_liquidate(&liquidator, &user, &50);
        assert!(res.is_err());
    }

    #[test]
    fn test_borrow_exactly_minimum_accepted() {
        let (_env, client, _admin, user) = setup();
        client.set_min_borrow(&50);
        let res = client.borrow(&user, &50);
        assert_eq!(res, 50);
    }

    #[test]
    fn test_set_min_borrow_admin_only() {
        let (_env, client, _admin, _user) = setup();
        assert_eq!(client.get_min_borrow(), 0);
        client.set_min_borrow(&100);
        assert_eq!(client.get_min_borrow(), 100);
    }

    #[test]
    #[should_panic(expected = "Unauthorized")]
    fn test_non_guardian_cannot_set_state() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&user, &EmergencyState::Shutdown);
    }

    #[test]
    #[should_panic(expected = "DepositNotAllowedInCurrentState")]
    fn test_shutdown_blocks_deposit() {
        let (_env, client, admin, user) = setup();
        client.set_emergency_state(&admin, &EmergencyState::Shutdown);
        client.deposit(&user, &10);
    }

    #[test]
    #[should_panic(expected = "BorrowNotAllowedInCurrentState")]
    fn test_shutdown_blocks_borrow() {
        let (_env, client, admin, user) = setup();
        client.set_emergency_state(&admin, &EmergencyState::Shutdown);
        client.borrow(&user, &5);
    }

    #[test]
    #[should_panic(expected = "WithdrawDisabledDuringShutdown")]
    fn test_shutdown_blocks_withdraw() {
        let (_env, client, admin, user) = setup();
        client.deposit(&user, &100);
        client.set_emergency_state(&admin, &EmergencyState::Shutdown);
        client.withdraw(&user, &10);
    }

    #[test]
    #[should_panic(expected = "RepayDisabledDuringShutdown")]
    fn test_shutdown_blocks_repay() {
        let (_env, client, admin, user) = setup();
        client.borrow(&user, &100);
        client.set_emergency_state(&admin, &EmergencyState::Shutdown);
        client.repay(&user, &10);
    }

    #[test]
    #[should_panic(expected = "DepositNotAllowedInCurrentState")]
    fn test_recovery_blocks_deposit() {
        let (_env, client, admin, user) = setup();
        client.set_emergency_state(&admin, &EmergencyState::Recovery);
        client.deposit(&user, &10);
    }

    #[test]
    #[should_panic(expected = "BorrowNotAllowedInCurrentState")]
    fn test_recovery_blocks_borrow() {
        let (_env, client, admin, user) = setup();
        client.set_emergency_state(&admin, &EmergencyState::Recovery);
        client.borrow(&user, &10);
    }

    #[test]
    fn test_recovery_allows_repay_and_withdraw() {
        let (_env, client, admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &50);
        client.set_emergency_state(&admin, &EmergencyState::Recovery);
        let repay_result = client.repay(&user, &10);
        assert_eq!(repay_result, 40);
        let withdraw_result = client.withdraw(&user, &10);
        assert_eq!(withdraw_result, 190);
    }
}
